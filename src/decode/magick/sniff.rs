//! Content sniffs that change how magick is invoked: metafiles, DICOM and the tiny AVIF.

/// The `-density` (DPI) that renders an EMF's LONG edge up to [`METAFILE_MIN_PX`] when its natural
/// (96-DPI) rasterization would be smaller — so a tiny clip-art EMF converts to a usable, crisp
/// image instead of ~64px. Returns None (magick's default density) when it's already big enough or
/// the frame is unreadable.
///
/// **EMF only, by design.** EMF's `ENHMETAHEADER.rclFrame` is authoritative — magick rasterizes
/// from it consistently, so the computed density matches the render. A *placeable WMF*'s header
/// bbox+`Inch` is NOT guaranteed to match the metafile body's own logical extents, so a
/// mismatched/hostile WMF header would make this compute a density that magick's WMF reader can't
/// honour (turning a file that decoded fine into a hard failure — caught in pre-1.0.1 review). WMF
/// is therefore left at its intrinsic size. The result is also capped ([`METAFILE_MAX_DENSITY`]) so
/// even an implausibly tiny declared EMF frame can't ask magick to build a canvas it chokes on.
pub(in super::super) fn metafile_min_density(b: &[u8]) -> Option<u32> {
    const METAFILE_MIN_PX: f64 = 512.0;
    const DEFAULT_DPI: f64 = 96.0;
    const METAFILE_MAX_DENSITY: u32 = 1200;
    if !(b.len() >= 44 && b[0..4] == [0x01, 0x00, 0x00, 0x00] && &b[40..44] == b" EMF") {
        return None; // not an EMF (placeable/memory WMF → intrinsic size, see doc above)
    }
    // rclFrame (4x i32, units of 0.01 mm; 2540 per inch) at offset 24.
    let i32_at = |o: usize| -> Option<f64> {
        Some(i32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?) as f64)
    };
    let w = (i32_at(32)? - i32_at(24)?).abs(); // right - left
    let h = (i32_at(36)? - i32_at(28)?).abs(); // bottom - top
    let long_inches = w.max(h) / 2540.0;
    if !long_inches.is_finite()
        || long_inches <= 0.0
        || long_inches * DEFAULT_DPI >= METAFILE_MIN_PX
    {
        return None; // unreadable, or already large enough at the default density
    }
    Some(((METAFILE_MIN_PX / long_inches).ceil() as u32).min(METAFILE_MAX_DENSITY))
}

/// Is this a Windows metafile (placeable/memory WMF, or EMF)? Selects the
/// metafile-specific limits for the magick tier and is the single home for the
/// metafile magic bytes — `container::looks_like_raster` also calls it so the
/// signatures live in exactly one place.
pub(crate) fn looks_like_metafile(b: &[u8]) -> bool {
    b.starts_with(&[0xD7, 0xCD, 0xC6, 0x9A])                    // placeable WMF
        || b.starts_with(&[0x01, 0x00, 0x09, 0x00, 0x00, 0x03]) // memory WMF METAHEADER
        || (b.len() >= 44 && b[0..4] == [0x01, 0x00, 0x00, 0x00] && &b[40..44] == b" EMF")
    // EMF
}

/// DICOM files carry a 128-byte preamble (often zero-filled) followed by the
/// magic "DICM" at offset 128.  The preamble is TIFF-compatible ("II*\0" at
/// offset 0 in many real-world samples including pydicom's CT_small.dcm and
/// MR_small.dcm), so ImageMagick's content-sniffer misidentifies them as TIFF
/// and fails ("Can not read TIFF directory count").  The explicit `dcm:-`
/// format hint in [`decode_via_magick`] routes them to the DICOM coder instead.
pub(super) fn looks_like_dicom(b: &[u8]) -> bool {
    b.len() > 132 && &b[128..132] == b"DICM"
}

/// Maximum number of top-level ISOBMFF boxes we inspect. Real AVIFs put `mini`
/// immediately after `ftyp`; the cap prevents a tiny-box flood from turning
/// this cheap routing predicate into an unbounded parser.
pub(super) const MAX_ISOBMFF_TOP_LEVEL_BOXES: usize = 64;

/// Return a checked top-level box's type, body start, and end offset.
/// `None` covers truncation, invalid lengths, and sizes that do not fit usize.
pub(super) fn isobmff_box_at(bytes: &[u8], offset: usize) -> Option<([u8; 4], usize, usize)> {
    let header = bytes.get(offset..offset.checked_add(8)?)?;
    let size32 = u32::from_be_bytes(header[0..4].try_into().ok()?);
    let typ = header[4..8].try_into().ok()?;
    let extended = if size32 == 1 {
        let raw = bytes.get(offset.checked_add(8)?..offset.checked_add(16)?)?;
        Some(u64::from_be_bytes(raw.try_into().ok()?))
    } else {
        None
    };
    let (size, header_len) = crate::container::boxhdr::decode_box_size(
        size32,
        extended,
        offset as u64,
        bytes.len() as u64,
    )?;
    let size = usize::try_from(size).ok()?;
    let header_len = usize::try_from(header_len).ok()?;
    Some((typ, offset + header_len, offset + size))
}

pub(super) fn is_mini_avif(bytes: &[u8]) -> bool {
    let Some((typ, body, mut offset)) = isobmff_box_at(bytes, 0) else {
        return false;
    };
    if typ != *b"ftyp" || !ftyp_describes_mini_avif(&bytes[body..offset]) {
        return false;
    }

    for _ in 0..MAX_ISOBMFF_TOP_LEVEL_BOXES {
        if offset == bytes.len() {
            return false;
        }
        let Some((typ, _, end)) = isobmff_box_at(bytes, offset) else {
            return false;
        };
        if typ == *b"mini" {
            return true;
        }
        offset = end;
    }
    false
}

/// A MinimizedImageBox file uses the `mif3` structural brand. For AV1, the
/// FileTypeBox minor-version word is the codec brand `avif` (ISO BMFF's
/// low-overhead-image amendment); it deliberately is not a compatible brand.
pub(super) fn ftyp_describes_mini_avif(body: &[u8]) -> bool {
    if body.len() < 8 || !(body.len() - 8).is_multiple_of(4) {
        return false;
    }
    let has_mif3 = body[..4] == *b"mif3"
        || body[8..]
            .as_chunks::<4>()
            .0
            .iter()
            .any(|brand| brand == b"mif3");
    has_mif3 && body[4..8] == *b"avif"
}
