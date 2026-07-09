//! Shared byte/string primitives for the container cover extractors.
//!
//! These run on attacker-controlled bytes inside Explorer's thumbnail host under
//! `panic = "abort"`, so every reader is bounds-checked (returns `Option`) and the
//! substring search guards an EMPTY needle — `windows(0)` panics in std, which here
//! would abort the shell host. Centralized so a hardening fix lands once instead of
//! in the 3–4 hand-copied versions that had already drifted (some lacked the guard).

/// Case-insensitive substring search. Guards an empty needle (which would make
/// `windows(0)` panic) and a needle longer than the haystack.
pub(super) fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    hay.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
}

/// First index of `needle` in `hay`, or None. Guards an empty needle (which would
/// make `windows(0)` panic) and a needle longer than the haystack.
pub(super) fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Accept `data` only if it looks like a raster format our image tiers can decode
/// (container previews are sometimes EMF/WMF, which we can't render).
pub(super) fn decodable_image(data: Vec<u8>) -> Option<Vec<u8>> {
    super::looks_like_raster(&data).then_some(data)
}

/// Big-endian `u16` at byte offset `o`, bounds-checked.
pub(super) fn be16(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_be_bytes([s[0], s[1]]))
}

/// Little-endian `u16` at byte offset `o`, bounds-checked.
pub(super) fn le16(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}

/// Wrap a bare Windows DIB (a `BITMAPINFOHEADER` + palette + pixels, with NO
/// `BM` file header) into a complete, decodable `.bmp` by prepending the 14-byte
/// `BITMAPFILEHEADER`. Used by the DWG / Rhino / 3ds-Max / CorelDRAW preview
/// extractors, whose embedded previews are stored as raw DIBs. Rejects a `biSize`
/// outside the known `BITMAPINFOHEADER`-family sizes and a `biBitCount` outside the
/// valid set, computes `bfOffBits` from the header (palette size for ≤8bpp, +12 for
/// `BI_BITFIELDS`), and bounds the wrapped output to [`super::MAX_COVER`].
pub(super) fn dib_to_bmp(dib: &[u8]) -> Option<Vec<u8>> {
    if dib.len() < 40 {
        return None;
    }
    let bi_size = le32(dib, 0)?;
    if !matches!(bi_size, 40 | 52 | 56 | 108 | 124) {
        return None; // not a BITMAPINFOHEADER-family DIB (we don't handle the old OS/2 core header)
    }
    let bit_count = le16(dib, 14)?;
    if !matches!(bit_count, 1 | 4 | 8 | 16 | 24 | 32) {
        return None;
    }
    let compression = le32(dib, 16)?;
    let clr_used = le32(dib, 32)?;
    let ncol = if clr_used != 0 {
        clr_used
    } else if bit_count <= 8 {
        1u32 << bit_count
    } else {
        0
    };
    let palette_bytes = ncol.checked_mul(4)?;
    let mask_bytes = if compression == 3 { 12 } else { 0 }; // BI_BITFIELDS masks
    let bf_off_bits = 14u32
        .checked_add(bi_size)?
        .checked_add(palette_bytes)?
        .checked_add(mask_bytes)?;
    let bf_size = 14u32.checked_add(u32::try_from(dib.len()).ok()?)?;
    let mut out = Vec::with_capacity(14 + dib.len());
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&bf_size.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]); // bfReserved1/2
    out.extend_from_slice(&bf_off_bits.to_le_bytes());
    out.extend_from_slice(dib);
    (out.len() as u64 <= super::MAX_COVER).then_some(out)
}

/// Big-endian `u32` at byte offset `o`, bounds-checked.
pub(super) fn be32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

/// Little-endian `u32` at byte offset `o`, bounds-checked.
pub(super) fn le32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Little-endian `u64` at byte offset `o`, bounds-checked. (ASF object sizes.)
pub(super) fn le64(b: &[u8], o: usize) -> Option<u64> {
    b.get(o..o + 8)
        .map(|s| u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
}

/// Total byte length (`SOI..EOI` inclusive) of the JPEG starting at `off`, or `None`
/// if it isn't well-formed. Skips marker segments by their declared length and scans
/// the entropy-coded stream with FF-stuffing / restart-marker awareness, so the real
/// EOI is found even when a metadata segment contains stray `FF D9` bytes. Fully
/// bounds-checked (`?` on every read) — never panics under `panic = "abort"`. Shared
/// by every embedded-JPEG carver (RAW previews, PSP composite bank, C4D scene
/// preview) — previously three hand-copies that had drifted to different segment
/// caps; this uses the strictest of the three (4096).
pub(crate) fn jpeg_span_len(data: &[u8], off: usize) -> Option<usize> {
    if data.get(off..off.checked_add(2)?)? != [0xFF, 0xD8] {
        return None;
    }
    let mut p = off + 2;
    // A well-formed JPEG has far fewer segments than this; the cap just stops a
    // crafted run of pseudo-markers from spinning.
    for _ in 0..4096 {
        if *data.get(p)? != 0xFF {
            return None; // expected a marker here
        }
        while *data.get(p)? == 0xFF {
            p = p.checked_add(1)?; // skip 0xFF fill bytes
        }
        let marker = *data.get(p)?;
        p = p.checked_add(1)?;
        match marker {
            0xD9 => return Some(p - off), // EOI — done
            0xDA => {
                // Start-of-scan: skip its header by length, then the entropy data.
                let len = u16::from_be_bytes([*data.get(p)?, *data.get(p + 1)?]) as usize;
                if len < 2 {
                    return None;
                }
                p = p.checked_add(len)?;
                loop {
                    if *data.get(p)? == 0xFF {
                        let n = *data.get(p + 1)?;
                        if n == 0x00 || (0xD0..=0xD7).contains(&n) {
                            p = p.checked_add(2)?; // byte-stuffed FF / restart marker
                            continue;
                        }
                        break; // a real marker (EOI, or next scan) — outer loop handles it
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

    #[test]
    fn contains_ci_guards_empty_needle_and_overlong() {
        // An empty needle must NOT panic (windows(0)) and must be false.
        assert!(!contains_ci(b"anything", b""));
        // Needle longer than haystack is false, not a panic.
        assert!(!contains_ci(b"hi", b"hello"));
        // Case-insensitive match still works.
        assert!(contains_ci(b"AbCOpenDocumentXyz", b"opendocument"));
        assert!(!contains_ci(b"nope", b"zzz"));
    }

    #[test]
    fn byte_readers_are_bounds_checked() {
        let b = [0x12u8, 0x34, 0x56, 0x78];
        assert_eq!(be16(&b, 0), Some(0x1234));
        assert_eq!(be32(&b, 0), Some(0x1234_5678));
        assert_eq!(le32(&b, 0), Some(0x7856_3412));
        // Out-of-range offsets return None, never panic.
        assert_eq!(be16(&b, 3), None);
        assert_eq!(be32(&b, 1), None);
        assert_eq!(le32(&b, 4), None);
    }
}
