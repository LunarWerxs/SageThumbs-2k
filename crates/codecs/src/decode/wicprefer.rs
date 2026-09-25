//! Which BMP, GIF and WebP files WIC decodes better than the `image` crate, decided from the header.

/// Is this a plain uncompressed BMP that the OS codec should decode ahead of the `image` tier?
///
/// BMP is the extreme case of "cheap to decode, expensive to materialise": there is no
/// decompression to speak of, so essentially the whole cost is turning 12 MP into pixels we
/// then throw away. WIC scales during the read; the `image` tier cannot. Measured on the
/// 12 MP tier: 258.6 ms ours against 22.1 ms Windows, and 2.3 ms against 0.6 ms at 0.08 MP —
/// the gap is entirely a function of size, which is why only the bounded-thumbnail callers
/// take this path and the full-fidelity ones are untouched.
///
/// The gate excludes the two places BMP decoders legitimately disagree, because a faster
/// thumbnail is worth nothing if it is a DIFFERENT thumbnail:
///
/// * **32-bit BMPs.** The fourth byte is alpha in some writers and padding full of garbage in
///   others; the format never settled it. `image` and WIC are entitled to read those files
///   differently, so they stay on the decoder whose output is already pinned by the corpus.
/// * **Compressed BMPs** (RLE4/RLE8, embedded JPEG/PNG). `BI_RGB` and `BI_BITFIELDS` are the
///   plain memory layouts this optimisation is about; the rest are their own decoders with
///   their own quirks, and they are never the large files this exists to speed up.
///
/// Anything unparseable is ineligible, so a truncated or lying header simply keeps the
/// existing tier order.
pub(super) fn bmp_prefers_wic(bytes: &[u8]) -> bool {
    // BITMAPFILEHEADER is 14 bytes, then the DIB header: size(4) width(4) height(4) planes(2)
    // bitcount(2) compression(4). A BITMAPCOREHEADER (12) has no compression field and no
    // 32-bit form, so it is excluded by the header-size check rather than special-cased.
    if bytes.len() < 54 || &bytes[0..2] != b"BM" {
        return false;
    }
    let dib_size = u32::from_le_bytes([bytes[14], bytes[15], bytes[16], bytes[17]]);
    if dib_size < 40 {
        return false;
    }
    let bitcount = u16::from_le_bytes([bytes[28], bytes[29]]);
    let compression = u32::from_le_bytes([bytes[30], bytes[31], bytes[32], bytes[33]]);
    const BI_RGB: u32 = 0;
    const BI_BITFIELDS: u32 = 3;
    matches!(bitcount, 1 | 4 | 8 | 16 | 24) && matches!(compression, BI_RGB | BI_BITFIELDS)
}

/// Step over an Extension block (`0x21`): one label byte, then a sub-block chain. Returns the
/// index just past it, or `None` if the file is truncated.
pub(super) fn gif_skip_extension(bytes: &[u8], i: usize) -> Option<usize> {
    if i >= bytes.len() {
        return None;
    }
    gif_skip_subblocks(bytes, i + 1)
}

/// Step over an Image Descriptor (`0x2C`) and verify it is a full-canvas frame: left/top zero
/// and width/height matching the logical screen. Returns the index just past it, or `None` if
/// the frame does not qualify (offset, undersized, or truncated) or the file is truncated.
pub(super) fn gif_full_canvas_descriptor(
    bytes: &[u8],
    screen_w: u16,
    screen_h: u16,
    i: usize,
) -> Option<usize> {
    let desc = bytes.get(i..i + 9)?;
    let left = u16::from_le_bytes([desc[0], desc[1]]);
    let top = u16::from_le_bytes([desc[2], desc[3]]);
    let w = u16::from_le_bytes([desc[4], desc[5]]);
    let h = u16::from_le_bytes([desc[6], desc[7]]);
    if left != 0 || top != 0 || w != screen_w || h != screen_h {
        return None;
    }
    let local_table = desc[8];
    let mut next = i + 9;
    if local_table & 0x80 != 0 {
        next += 3 << ((local_table & 0x07) + 1);
    }
    if next >= bytes.len() {
        return None;
    }
    gif_skip_subblocks(bytes, next + 1)
}

/// Is this a SINGLE-FRAME GIF whose one frame covers the whole logical screen, so the OS
/// codec should decode it ahead of the `image` tier?
///
/// GIF was the worst ratio in the entire speed baseline: 6.8 ms against Windows' 0.5 ms at
/// 0.08 MP (14.7x) and 306.5 ms against 50.5 ms at 12 MP. Nothing about LZW is slow; the
/// cost is the same one BMP and WebP had, which is materialising every pixel of a picture
/// that is about to be shrunk to 256 px. WIC scales during the read.
///
/// The gate walks the block chain rather than trusting the header, and refuses on anything
/// it cannot account for, because the two decoders are only interchangeable in the plain case:
///
/// * **More than one image descriptor.** An animation's thumbnail is a FRAME CHOICE, and a
///   frame choice is the decoder's, not ours to change for a speed win. The same reasoning
///   keeps animated WebP off its fast path.
/// * **A frame that does not cover the logical screen.** The `image` tier composites the
///   frame onto the full-size canvas; WIC hands back the frame at its OWN size. Identical
///   for a normal still, a different picture for an offset or undersized one.
/// * **Anything unparseable or truncated**, which simply keeps the existing tier order.
pub(super) fn gif_prefers_wic(bytes: &[u8]) -> bool {
    if bytes.len() < 13 || (&bytes[0..6] != b"GIF87a" && &bytes[0..6] != b"GIF89a") {
        return false;
    }
    let screen_w = u16::from_le_bytes([bytes[6], bytes[7]]);
    let screen_h = u16::from_le_bytes([bytes[8], bytes[9]]);
    // Logical Screen Descriptor packed byte: bit 7 global colour table present, bits 0-2 its
    // size as 3 * 2^(n+1) bytes. Then the background-colour index and pixel aspect ratio.
    let packed = bytes[10];
    let mut i = 13usize;
    if packed & 0x80 != 0 {
        i += 3 << ((packed & 0x07) + 1);
    }
    let mut frames = 0u32;
    loop {
        let Some(&marker) = bytes.get(i) else {
            return false;
        };
        i += 1;
        match marker {
            // Trailer: eligible only if exactly one frame was seen and it was a full-canvas one.
            0x3B => return frames == 1,
            // Extension: one label byte, then a sub-block chain.
            0x21 => {
                let Some(next) = gif_skip_extension(bytes, i) else {
                    return false;
                };
                i = next;
            }
            // Image descriptor: left, top, width, height (2 bytes each) then a packed byte
            // whose bit 7 is a local colour table and bits 0-2 its size, then the LZW minimum
            // code size, then the compressed sub-block chain.
            0x2C => {
                frames += 1;
                if frames > 1 {
                    return false;
                }
                let Some(next) = gif_full_canvas_descriptor(bytes, screen_w, screen_h, i) else {
                    return false;
                };
                i = next;
            }
            _ => return false,
        }
    }
}

/// Step over one GIF sub-block chain (length-prefixed runs ended by a zero length) and
/// return the index just past its terminator, or `None` if it runs off the end. Every step
/// advances `i`, so a hostile file cannot spin here.
pub(super) fn gif_skip_subblocks(bytes: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let n = *bytes.get(i)? as usize;
        i = i.checked_add(1)?.checked_add(n)?;
        if n == 0 {
            return Some(i);
        }
    }
}

/// Is this a STILL, non-ICC WebP that the OS codec should decode ahead of the `image` tier?
///
/// The gate is deliberately narrow, and each exclusion is load-bearing:
/// * `VP8 `/`VP8L` directly after the RIFF header — a simple still with no feature flags at
///   all — is always eligible.
/// * `VP8X` is eligible only with the ANIMATION and ICC bits clear. Animated WebP must stay
///   on the pure-Rust path because which frame becomes the thumbnail is the DECODER's choice
///   and `sample-decoy-frames.webp` pins that choice; ICC-tagged WebP stays because colour
///   management is verified on the current path and unverified through WIC.
/// * Anything unparseable is ineligible, so a truncated or lying header simply keeps the
///   existing tier order.
///
/// VP8X flags byte (WebP container spec): `RR I L E X A R` — bit 5 ICC, bit 4 alpha,
/// bit 3 EXIF, bit 2 XMP, bit 1 animation. Alpha/EXIF/XMP stay eligible: WIC preserves the
/// alpha plane through the same 32bppRGBA conversion every other WIC format uses, and EXIF
/// orientation is applied by our own pipeline from the file bytes, identically on either
/// decode path.
pub(super) fn webp_prefers_wic(bytes: &[u8]) -> bool {
    if bytes.len() < 21 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return false;
    }
    match &bytes[12..16] {
        b"VP8 " | b"VP8L" => true,
        b"VP8X" => {
            const ICC: u8 = 0x20;
            const ANIM: u8 = 0x02;
            bytes[20] & (ICC | ANIM) == 0
        }
        _ => false,
    }
}
