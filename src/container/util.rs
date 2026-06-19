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
/// `BITMAPFILEHEADER`. Used by the DWG / Rhino / 3ds-Max preview extractors, whose
/// embedded previews are stored as raw DIBs. Computes `bfOffBits` from the header
/// (palette size for ≤8bpp, +12 for `BI_BITFIELDS`). Returns None on a malformed
/// or non-`BITMAPINFOHEADER` DIB.
pub(super) fn dib_to_bmp(dib: &[u8]) -> Option<Vec<u8>> {
    let bi_size = le32(dib, 0)?;
    if bi_size < 40 {
        return None; // not a BITMAPINFOHEADER (we don't handle the old OS/2 core header)
    }
    let bit_count = le16(dib, 14)?;
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
    Some(out)
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
