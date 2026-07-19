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
///
/// This is the CHEAP path and only finds JPEG-compressed composites. Many PSP files store the
/// composite with LZ77 instead and contain no JPEG at all (every `.PspBrush` observed so far);
/// [`extract_best`] handles those and should be preferred by callers.
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

// ===== Full Composite Image Bank parse (LZ77 / uncompressed channel data) =====
//
// Added 2026-07-18 after a community report (issue #4, @LeviFiction) supplying real
// `.PspBrush` / `.PspTube` samples. Two things the JPEG-only path got wrong:
//
//   * `.PspBrush` stores its composite as an 8-bit PALETTED, LZ77-compressed image and carries
//     no JPEG whatsoever, so brushes produced no thumbnail at all.
//   * `.PspTube` carries BOTH an 80x80 JPEG thumbnail and the real 900x900 LZ77 composite. The
//     JPEG carve found the 80x80 one, so tubes thumbnailed at a fraction of the available
//     resolution.
//
// PSP's "LZ77" is plain zlib (verified: streams start `78 9C`). Per the format spec it is the
// PNG LZ77 variant WITHOUT PNG's per-scanline filters and restricted to one contiguous stream,
// which is exactly a bare zlib stream over raw rows. `flate2` is already a dependency.

const COLOR_BLOCK: u16 = 2;
const CHANNEL_BLOCK: u16 = 5;
const COMPOSITE_IMAGE_BLOCK: u16 = 9;
const COMPOSITE_ATTRIBUTES: u16 = 17;

const JPEG_SUBBLOCK: u16 = 18;

const COMP_NONE: u16 = 0;
const COMP_LZ77: u16 = 2;
const COMP_JPEG: u16 = 3;

const CHAN_COMPOSITE: u16 = 0;
const CHAN_RED: u16 = 1;
const CHAN_GREEN: u16 = 2;
const CHAN_BLUE: u16 = 3;

/// Cap on one composite's pixel count. A preview is never legitimately larger, and this bounds
/// every allocation below on attacker-controlled dimensions (`w * h` cannot overflow usize on
/// 64-bit at this magnitude, and each channel buffer is at most this many bytes).
const MAX_PIXELS: usize = 32 * 1024 * 1024;

/// One entry of the Composite Image Bank's attributes list.
#[derive(Clone, Copy)]
struct Attrs {
    w: u32,
    h: u32,
    depth: u16,
    compression: u16,
}

/// Iterate a block's sub-blocks, skipping its leading info chunk.
///
/// LOAD-BEARING: a bank/composite block's content begins with a `u32` chunk length that
/// INCLUDES ITSELF, and sub-blocks start only after it. Walking from the content start instead
/// lands mid-chunk and every subsequent field is garbage.
fn sub_blocks(b: &[u8], content: usize, end: usize) -> Vec<(u16, usize, usize)> {
    let mut out = Vec::new();
    let Some(chunk) = read_u32(b, content) else { return out };
    let Some(mut p) = content.checked_add(chunk as usize) else { return out };
    // A malformed chunk length could point past `end`; the loop guard catches that.
    while p + BLOCK_PREFIX <= end && out.len() < 64 {
        if b.get(p..p + 4) != Some(&BK[..]) {
            break;
        }
        let id = u16::from_le_bytes([b[p + 4], b[p + 5]]);
        let len = u32::from_le_bytes([b[p + 6], b[p + 7], b[p + 8], b[p + 9]]) as usize;
        let c = p + BLOCK_PREFIX;
        let Some(next) = c.checked_add(len) else { break };
        if next > end {
            break;
        }
        out.push((id, c, len));
        p = next;
    }
    out
}

fn read_u32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn read_u16(b: &[u8], at: usize) -> Option<u16> {
    let s = b.get(at..at + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

/// Inflate a zlib stream, refusing to produce more than `limit` bytes so a compression bomb
/// can't exhaust memory inside the shell.
fn inflate_capped(data: &[u8], limit: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .take(limit as u64)
        .read_to_end(&mut out)
        .ok()?;
    (!out.is_empty()).then_some(out)
}

/// Decode the best composite image in the bank: the largest by pixel area, whether it is stored
/// as JPEG (returned as bytes for the normal decoder) or as raw/LZ77 channel planes (decoded
/// here). `None` if the file has no usable composite, in which case the caller falls back to
/// [`extract`]'s bounded JPEG carve.
pub fn extract_best(bytes: &[u8]) -> Option<crate::container::CoverOut> {
    if !looks_like_psp(bytes) {
        return None;
    }
    let (lo, hi) = composite_bank_range(bytes)?;

    // Attributes come first, in order, one per composite; the image blocks follow in the SAME
    // order. Pair them by index rather than by any id, which is how the format expresses it.
    let mut attrs: Vec<Attrs> = Vec::new();
    let mut jpegs: Vec<usize> = Vec::new(); // index into `blocks` order
    let mut planes: Vec<(usize, usize)> = Vec::new(); // (content, len) of composite image blocks
    for (id, c, len) in sub_blocks(bytes, lo, hi) {
        match id {
            COMPOSITE_ATTRIBUTES => {
                // chunk(4) w(4) h(4) depth(2) compression(2) planes(2) colors(4) type(2)
                let (Some(w), Some(h)) = (read_u32(bytes, c + 4), read_u32(bytes, c + 8)) else {
                    continue;
                };
                let (Some(depth), Some(compression)) =
                    (read_u16(bytes, c + 12), read_u16(bytes, c + 14))
                else {
                    continue;
                };
                attrs.push(Attrs { w, h, depth, compression });
            }
            JPEG_SUBBLOCK => jpegs.push(c),
            COMPOSITE_IMAGE_BLOCK => planes.push((c, len)),
            _ => {}
        }
    }

    // Rank every composite we could actually produce, largest first.
    let mut best: Option<(usize, usize)> = None; // (pixels, attrs index)
    for (i, a) in attrs.iter().enumerate() {
        let px = (a.w as usize).checked_mul(a.h as usize)?;
        if px == 0 || px > MAX_PIXELS {
            continue;
        }
        if best.is_none_or(|(bp, _)| px > bp) {
            best = Some((px, i));
        }
    }
    let (_, want) = best?;
    let a = *attrs.get(want)?;

    // A JPEG composite: hand the bytes back and let the normal tier decode them. `jpegs` and
    // `planes` are each in bank order, and attributes are too, so the Nth attributes entry of a
    // given storage kind lines up with the Nth block of that kind.
    if a.compression == COMP_JPEG {
        let rank = attrs.iter().take(want).filter(|x| x.compression == COMP_JPEG).count();
        if let Some(&c) = jpegs.get(rank) {
            // The JPEG sub-block content is chunk(4) then the JPEG stream.
            let chunk = read_u32(bytes, c)? as usize;
            let data = bytes.get(c + chunk..)?;
            let len = crate::container::jpeg_span_len(data, 0)?;
            let jpeg = data.get(..len)?;
            if jpeg.len() >= 64 && jpeg.len() as u64 <= crate::container::MAX_COVER {
                return Some(crate::container::CoverOut::Bytes(jpeg.to_vec()));
            }
        }
        return None;
    }

    let rank = attrs.iter().take(want).filter(|x| x.compression != COMP_JPEG).count();
    let &(pc, plen) = planes.get(rank)?;
    decode_channels(bytes, pc, plen, &a).map(crate::container::CoverOut::Image)
}

/// Decode one Composite Image Block's channel planes into an RGB image.
///
/// Handles the two layouts these files actually use: 8-bit PALETTED (a single `COMPOSITE`
/// channel indexing the sibling `COLOR` block's table) and 24-bit RGB (separate `RED`/`GREEN`/
/// `BLUE` planes). Compression is LZ77 (zlib) or none. RLE is deliberately unhandled — no
/// sample exercising it exists, and guessing at a codec that nothing verifies is worse than
/// falling through to the JPEG carve.
fn decode_channels(b: &[u8], content: usize, len: usize, a: &Attrs) -> Option<image::DynamicImage> {
    let px = (a.w as usize).checked_mul(a.h as usize)?;
    if px == 0 || px > MAX_PIXELS {
        return None;
    }
    let mut palette: Option<&[u8]> = None;
    let mut chan: [Option<Vec<u8>>; 4] = [None, None, None, None];

    for (id, c, sub_len) in sub_blocks(b, content, content.checked_add(len)?) {
        match id {
            COLOR_BLOCK => {
                // chunk(4) entryCount(4) then `count` BGRA quads (Windows RGBQUAD order).
                let n = read_u32(b, c + 4)? as usize;
                if n > 0 && n <= 256 {
                    palette = b.get(c + 8..c + 8 + n * 4);
                }
            }
            CHANNEL_BLOCK => {
                // chunk(4) compressedLen(4) uncompressedLen(4) bitmapType(2) channelType(2)
                let chunk = read_u32(b, c)? as usize;
                let clen = read_u32(b, c + 4)? as usize;
                let ctype = read_u16(b, c + 14)?;
                let data = b.get(c + chunk..c + chunk.checked_add(clen)?)?;
                let raw = match a.compression {
                    COMP_LZ77 => inflate_capped(data, px)?,
                    COMP_NONE => data.get(..px.min(data.len()))?.to_vec(),
                    _ => return None, // RLE or unknown: let the caller fall back
                };
                if raw.len() < px {
                    return None; // truncated plane — do not render a half image
                }
                if let Some(slot) = chan.get_mut(ctype as usize) {
                    *slot = Some(raw);
                }
                let _ = sub_len;
            }
            _ => {}
        }
    }

    let mut rgb = vec![0u8; px.checked_mul(3)?];
    match a.depth {
        8 => {
            let idx = chan[CHAN_COMPOSITE as usize].as_ref()?;
            let pal = palette?;
            for i in 0..px {
                let e = idx[i] as usize * 4;
                // BGRA in the file; missing/short entries render black rather than panicking.
                let (bl, g, r) = (
                    pal.get(e).copied().unwrap_or(0),
                    pal.get(e + 1).copied().unwrap_or(0),
                    pal.get(e + 2).copied().unwrap_or(0),
                );
                rgb[i * 3] = r;
                rgb[i * 3 + 1] = g;
                rgb[i * 3 + 2] = bl;
            }
        }
        24 | 32 => {
            let r = chan[CHAN_RED as usize].as_ref()?;
            let g = chan[CHAN_GREEN as usize].as_ref()?;
            let bl = chan[CHAN_BLUE as usize].as_ref()?;
            for i in 0..px {
                rgb[i * 3] = r[i];
                rgb[i * 3 + 1] = g[i];
                rgb[i * 3 + 2] = bl[i];
            }
        }
        // Greyscale composites index no palette; replicate the single plane.
        1 | 4 => return None, // sub-byte packing, no sample to verify against
        _ => return None,
    }
    image::RgbImage::from_raw(a.w, a.h, rgb).map(image::DynamicImage::ImageRgb8)
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

    /// Real `.PspBrush` / `.PspTube` files supplied by @LeviFiction on issue #4. These are the
    /// samples that showed the JPEG-only path was wrong, so they are the regression:
    ///
    ///   * the brush stores an 8-bit PALETTED, LZ77 composite and contains NO JPEG at all —
    ///     before the bank parse it produced no thumbnail whatsoever;
    ///   * the tube carries an 80x80 JPEG thumbnail AND the real 900x900 LZ77 composite, so
    ///     the JPEG carve "worked" while silently throwing away 99% of the resolution.
    ///
    /// SKIPS (does not fail) when the corpus is absent. The corpus is a LOCAL-ONLY sibling
    /// directory, never committed, so CI clones do not have it — an earlier version of this
    /// test asserted it had run and broke CI on exactly that. The samples are also
    /// contributor-supplied files shared for testing; redistributing them in this repo is not
    /// ours to decide, so they deliberately stay out of tree.
    #[test]
    fn decodes_real_psp_family_samples_via_lz77() {
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../test-corpus");
        // (file, expected dimensions, expected mostly-white background)
        let cases = [("blob.PspBrush", 300u32, 300u32), ("countdown.PspTube", 900, 900)];
        let mut ran = 0;
        for (name, w, h) in cases {
            let p = corpus.join(name);
            let Ok(bytes) = std::fs::read(&p) else { continue };
            ran += 1;
            let cover = crate::container::extract_cover(&bytes)
                .unwrap_or_else(|| panic!("{name}: no cover extracted"));
            let img = match cover {
                crate::container::CoverOut::Image(i) => i,
                crate::container::CoverOut::Bytes(b) => image::load_from_memory(&b)
                    .unwrap_or_else(|e| panic!("{name}: carved bytes not decodable: {e}")),
            };
            assert_eq!(
                (img.width(), img.height()), (w, h),
                "{name}: wrong dimensions - a tube regressing to 80x80 means the JPEG carve \
                 won again and the full composite was skipped",
            );
            // Both samples are dark art on a white ground. This catches a channel-order or
            // palette-order mistake, which would otherwise still produce a right-sized image.
            let rgb = img.to_rgb8();
            let corner = rgb.get_pixel(2, 2).0;
            assert!(
                corner.iter().all(|&c| c > 200),
                "{name}: top-left should be near-white background, got {corner:?}",
            );
            let dark = rgb.pixels().filter(|p| p.0.iter().all(|&c| c < 64)).count();
            assert!(dark > 1000, "{name}: expected substantial dark artwork, got {dark} px");
        }
        if ran == 0 {
            // Loud on purpose: a silently-skipping test is one that can rot unnoticed. This
            // says plainly that it verified NOTHING on this machine, rather than passing green
            // and implying coverage it does not have.
            eprintln!(
                "SKIPPED: no PSP samples under {} — this test verified NOTHING. \
                 It only has teeth on a machine with the local test corpus.",
                corpus.display()
            );
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
