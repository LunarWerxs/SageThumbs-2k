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
    hay.windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
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
    b.get(o..o + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

/// Little-endian `u32` at byte offset `o`, bounds-checked.
pub(super) fn le32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
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
    jpeg_span(data, off).map(|(len, _)| len)
}

/// Is `sof` a frame the raster decoders in this codebase can actually decode?
///
/// Only the DCT-based, Huffman-coded frames: SOF0 (baseline), SOF1 (extended sequential),
/// SOF2 (progressive). Everything else in the SOFn range is a dead end here — the arithmetic
/// variants (SOF9/10/11, 13-15) are unimplemented in the `image` crate and in WIC, the
/// differential ones (SOF5-7) are effectively extinct, and **SOF3 is lossless JPEG, which is
/// how Canon CR2 (and many DNGs) store the compressed SENSOR DATA**.
///
/// That last one is not a curiosity, it is the whole reason this function exists. A CR2's raw
/// stream is a ~20 MB "JPEG" by every structural test — valid SOI, valid marker chain, valid
/// EOI — so a scanner looking for the largest embedded JPEG picks it over the real 3 MB
/// display preview and hands a decoder something no decoder can read. See
/// [`crate::decode::tiers::largest_embedded_jpeg`].
pub(crate) fn jpeg_sof_is_decodable(sof: u8) -> bool {
    matches!(sof, 0xC0..=0xC2)
}

/// [`jpeg_span_len`] plus the frame's SOF marker, so a caller can tell a picture from a
/// pile of sensor readings. Returns `(span length, SOF marker)`.
///
/// The SOF is an `Option` and deliberately does NOT gate the span: `jpeg_span_len` predates
/// this and several callers (`c4d`, `psp`) rely on its exact acceptance, so a stream whose
/// markers parse to a clean EOI without a frame header keeps measuring the same length it
/// always did. Only a caller that CARES what kind of frame it found consults the marker, and
/// then absence means "unknown", not "reject".
/// Skip a length-prefixed marker segment: reads its big-endian length (the field covers
/// its own 2 bytes) and returns the position right after the segment. `None` if the length
/// field is unreadable or claims less than its own 2 bytes.
fn skip_length_prefixed_segment(data: &[u8], p: usize) -> Option<usize> {
    let len = u16::from_be_bytes([*data.get(p)?, *data.get(p + 1)?]) as usize;
    if len < 2 {
        return None;
    }
    p.checked_add(len)
}

/// Skip the entropy-coded scan data after an SOS header, honoring FF-stuffing and restart
/// markers (`FF 00` and `FF D0..D7` are DATA, not the next real marker). Returns the
/// position of the next real `0xFF` marker byte.
fn skip_entropy_coded_scan(data: &[u8], mut p: usize) -> Option<usize> {
    loop {
        if *data.get(p)? == 0xFF {
            let n = *data.get(p + 1)?;
            if n == 0x00 || (0xD0..=0xD7).contains(&n) {
                p = p.checked_add(2)?; // byte-stuffed FF / restart marker
                continue;
            }
            return Some(p); // a real marker (EOI, or next scan) — outer loop handles it
        }
        p = p.checked_add(1)?;
    }
}

pub(crate) fn jpeg_span(data: &[u8], off: usize) -> Option<(usize, Option<u8>)> {
    if data.get(off..off.checked_add(2)?)? != [0xFF, 0xD8] {
        return None;
    }
    let mut sof: Option<u8> = None;
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
            0xD9 => return Some((p - off, sof)), // EOI — done
            // SOFn. 0xC4 (DHT), 0xC8 (reserved) and 0xCC (DAC) share the range but are not
            // frame headers; record the FIRST real one, since that is the frame this span
            // describes and a later thumbnail SOF must not overwrite it.
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                sof.get_or_insert(marker);
                p = skip_length_prefixed_segment(data, p)?;
            }
            0xDA => {
                // Start-of-scan: skip its header by length, then the entropy data.
                p = skip_length_prefixed_segment(data, p)?;
                p = skip_entropy_coded_scan(data, p)?;
            }
            0x01 | 0xD0..=0xD7 => {} // standalone markers, no payload
            _ => {
                p = skip_length_prefixed_segment(data, p)?;
            }
        }
    }
    None
}

/// Width/height (in that order) of the first decodable SOF frame (SOF0/1/2) in the
/// JPEG starting at `off`, or `None` if none is found before the walk's segment cap.
///
/// Shares [`jpeg_span`]'s bounded FF-marker walk (same 4096-segment cap) so this can't be
/// driven past the point that one is bounded at either; it stops as soon as it has the
/// dimensions rather than continuing to the EOI, since callers that only want a preview's
/// pixel size (not its byte span) have no use for walking the rest of the entropy data.
/// Extracted so cover extractors that need dimensions (not just a span) don't each hand-roll
/// their own marker walk — see `c4d.rs`'s `jpeg_dims`, previously a parallel copy of this walk.
pub(super) fn jpeg_sof_dims(data: &[u8], off: usize) -> Option<(u16, u16)> {
    if data.get(off..off.checked_add(2)?)? != [0xFF, 0xD8] {
        return None;
    }
    let mut p = off + 2;
    for _ in 0..4096 {
        let (marker, next) = next_marker(data, p)?;
        p = next;
        match marker {
            0xC0..=0xC2 => return sof0_dims(data, p),
            0xD9 | 0xDA => return None, // EOI or scan start reached with no frame header
            0x01 | 0xD0..=0xD7 => {}    // standalone markers, no payload
            _ => {
                p = skip_length_prefixed_segment(data, p)?;
            }
        }
    }
    None
}

/// Skip 0xFF fill bytes starting at `p` and read the marker byte that follows.
/// Returns `(marker, position right after the marker byte)`.
fn next_marker(data: &[u8], p: usize) -> Option<(u8, usize)> {
    if *data.get(p)? != 0xFF {
        return None; // expected a marker here
    }
    let mut p = p;
    while *data.get(p)? == 0xFF {
        p = p.checked_add(1)?; // skip 0xFF fill bytes
    }
    let marker = *data.get(p)?;
    Some((marker, p.checked_add(1)?))
}

/// An SOF0/1/2 frame header's width/height, read right after its marker byte at `p`
/// (length(2) precision(1) height(2) width(2)).
fn sof0_dims(data: &[u8], p: usize) -> Option<(u16, u16)> {
    let h = u16::from_be_bytes([*data.get(p + 3)?, *data.get(p + 4)?]);
    let w = u16::from_be_bytes([*data.get(p + 5)?, *data.get(p + 6)?]);
    Some((w, h))
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
    fn jpeg_sof_dims_reads_the_first_baseline_frame() {
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(37, 41))
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        assert_eq!(jpeg_sof_dims(&jpeg, 0), Some((37, 41)));
        // Not a JPEG at all -> None, not a panic.
        assert_eq!(jpeg_sof_dims(b"not a jpeg", 0), None);
        // Truncated right after SOI -> None (no marker to read).
        assert_eq!(jpeg_sof_dims(&jpeg[..2], 0), None);
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
