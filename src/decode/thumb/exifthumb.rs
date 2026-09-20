//! The EXIF side of a thumbnail: the embedded IFD1 preview and the orientation tag.

use super::*;

/// Decode a JPEG's embedded EXIF thumbnail (if any), applying the file's EXIF
/// orientation so it matches the full image. Best-effort: any malformation or
/// absence yields None and the caller does a full decode.
pub(in super::super) fn embedded_thumbnail(bytes: &[u8]) -> Option<DynamicImage> {
    let jpeg = exif_thumbnail_jpeg(bytes)?;
    let img = decode_with_image(jpeg).ok()?;
    Some(apply_exif_orientation(img, bytes))
}

/// Find the embedded thumbnail JPEG inside a JPEG's APP1/"Exif\0\0" segment and
/// return a slice of `bytes` covering that thumbnail's own JPEG stream.
pub(in super::super) fn exif_thumbnail_jpeg(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.get(0..2)? != [0xFF, 0xD8] {
        return None; // not a JPEG → no EXIF thumbnail to find
    }
    let mut i = 2usize;
    loop {
        let (marker, body_start, seg_end) = jpeg_segment(bytes, i)?;
        // Match the "Exif\0\0" id ONLY within this segment's own body — never
        // read past seg_end. Confining it here also guarantees body_start+6 <=
        // seg_end whenever it matches, so the slice below can't be start>end
        // (which would panic — and under panic=abort that aborts the host).
        if marker == 0xE1 && bytes.get(body_start..seg_end)?.starts_with(b"Exif\0\0") {
            return tiff_thumbnail(bytes.get(body_start + 6..seg_end)?);
        }
        i = seg_end;
    }
}

/// Parse one JPEG marker segment starting at `i`, returning `(marker, body_start, seg_end)`.
/// `None` means the caller is past the metadata headers (truncated input, a byte that is not
/// a marker, or the EOI / start-of-scan marker) — the point at which no EXIF thumbnail can
/// follow. The segment length includes its own two length bytes, so `seg_len < 2` is malformed.
pub(super) fn jpeg_segment(bytes: &[u8], i: usize) -> Option<(u8, usize, usize)> {
    // Each marker is 0xFF <marker> <len-hi> <len-lo> ...
    if *bytes.get(i)? != 0xFF {
        return None;
    }
    let marker = *bytes.get(i + 1)?;
    if marker == 0xD9 || marker == 0xDA {
        return None; // EOI / start-of-scan: past the metadata headers
    }
    let seg_len = u16::from_be_bytes([*bytes.get(i + 2)?, *bytes.get(i + 3)?]) as usize;
    if seg_len < 2 {
        return None;
    }
    let body_start = i + 4;
    let seg_end = i + 2 + seg_len;
    if seg_end > bytes.len() {
        return None;
    }
    Some((marker, body_start, seg_end))
}

#[inline]
pub(in super::super) fn r16(b: &[u8], off: usize, le: bool) -> Option<u16> {
    let s = b.get(off..off + 2)?;
    Some(if le {
        u16::from_le_bytes([s[0], s[1]])
    } else {
        u16::from_be_bytes([s[0], s[1]])
    })
}

#[inline]
pub(in super::super) fn r32(b: &[u8], off: usize, le: bool) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    Some(if le {
        u32::from_le_bytes([s[0], s[1], s[2], s[3]])
    } else {
        u32::from_be_bytes([s[0], s[1], s[2], s[3]])
    })
}

/// Walk the TIFF block (IFD0 → IFD1) for the thumbnail offset (0x0201) and
/// length (0x0202), returning the embedded JPEG slice. All offsets are relative
/// to the TIFF header (`tiff[0]`). Fully bounds-checked — never panics.
pub(in super::super) fn tiff_thumbnail(tiff: &[u8]) -> Option<&[u8]> {
    let (le, ifd0) = tiff_header(tiff)?;
    let (off, len) = ifd1_thumbnail_range(tiff, le, ifd0)?;
    let end = off.checked_add(len)?;
    let thumb = tiff.get(off..end)?;
    // Sanity: a real embedded thumbnail is itself a JPEG.
    if thumb.get(0..2)? == [0xFF, 0xD8] {
        Some(thumb)
    } else {
        None
    }
}

/// Read a TIFF header's byte order and its IFD0 offset, rejecting anything that is not a
/// big/little-endian TIFF (the `42` magic) or is too short to hold the header.
pub(super) fn tiff_header(tiff: &[u8]) -> Option<(bool, usize)> {
    let le = match tiff.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    if r16(tiff, 2, le)? != 42 {
        return None;
    }
    let ifd0 = r32(tiff, 4, le)? as usize;
    Some((le, ifd0))
}

/// Walk IFD0's IFD1 pointer and then IFD1's own entry table for the thumbnail offset (0x0201)
/// and length (0x0202), returning that `(offset, length)` pair. All offsets are relative to the
/// TIFF header (`tiff[0]`).
pub(super) fn ifd1_thumbnail_range(tiff: &[u8], le: bool, ifd0: usize) -> Option<(usize, usize)> {
    // IFD1 pointer follows IFD0's entries.
    let n0 = r16(tiff, ifd0, le)? as usize;
    let ifd1 = r32(tiff, ifd0 + 2 + n0 * 12, le)? as usize;
    if ifd1 == 0 {
        return None;
    }

    let n1 = r16(tiff, ifd1, le)? as usize;
    let (mut off, mut len) = (None, None);
    for e in 0..n1 {
        let entry = ifd1 + 2 + e * 12;
        match r16(tiff, entry, le)? {
            0x0201 => off = Some(r32(tiff, entry + 8, le)? as usize), // JPEGInterchangeFormat
            0x0202 => len = Some(r32(tiff, entry + 8, le)? as usize), // …Length
            _ => {}
        }
    }
    Some((off?, len?))
}

/// Map the 8 EXIF orientation values onto `image` transforms. Phone JPEGs
/// commonly use value 6 (rotate 90° CW). `rotate90` here is clockwise.
pub(in super::super) fn apply_exif_orientation(img: DynamicImage, bytes: &[u8]) -> DynamicImage {
    match exif_orientation(bytes) {
        Some(2) => img.fliph(),
        Some(3) => img.rotate180(),
        Some(4) => img.flipv(),
        Some(5) => img.rotate90().fliph(),
        Some(6) => img.rotate90(),
        Some(7) => img.rotate270().fliph(),
        Some(8) => img.rotate270(),
        _ => img,
    }
}

/// EXIF Orientation (tag `0x0112`) straight out of IFD0, walking only the entry table
/// with the existing [`r16`]/[`r32`] helpers — the bounded, zero-copy sibling of
/// [`tiff_thumbnail`]'s IFD1 walk, for the one question this call site actually needs
/// answered. `exif::Reader::read_from_container` reads a TIFF-magic buffer whole to
/// answer this same one-tag question, and every camera RAW this handler is hooked for
/// IS a TIFF container, so this is what keeps a 150 MB scanner TIFF (or a multi-GB RAW)
/// from paying that internal cost just to orient a thumbnail. Fully bounds-checked —
/// never panics on a truncated or hostile IFD.
pub(super) fn tiff_ifd0_orientation(tiff: &[u8]) -> Option<u32> {
    let (le, ifd0) = tiff_header(tiff)?;
    ifd0_orientation(tiff, le, ifd0)
}

/// Orientation (tag `0x0112`) from IFD0's entry table, walking it with the bounded [`r16`]
/// helpers. `None` when the table has no Orientation entry or is truncated.
pub(super) fn ifd0_orientation(tiff: &[u8], le: bool, ifd0: usize) -> Option<u32> {
    let n0 = r16(tiff, ifd0, le)? as usize;
    for e in 0..n0 {
        let entry = ifd0.checked_add(2)?.checked_add(e.checked_mul(12)?)?;
        if r16(tiff, entry, le)? != 0x0112 {
            continue;
        }
        // Orientation is SHORT (type 3) and left-justified in the 4-byte value field in
        // both endiannesses, the same read `rawsniff.rs`'s NewSubfileType entry uses.
        return r16(tiff, entry.checked_add(8)?, le).map(u32::from);
    }
    None
}

pub(crate) fn exif_orientation(bytes: &[u8]) -> Option<u32> {
    // Magic-gate before handing the bytes to `exif::Reader`: it only reads EXIF from
    // JPEG / TIFF / PNG / WebP / HEIF, returning an error (→ None) for anything else.
    // Skipping the reader setup for the formats it can't read (GIF/BMP/ICO/QOI/TGA/
    // PNM/DDS/…) is behavior-identical and saves a parse attempt on every such
    // thumbnail. (PNG/WebP/HEIF stay in — they CAN carry an EXIF orientation.)
    if !has_exif_container(bytes) {
        return None;
    }
    // TIFF magic (classic and camera-RAW TIFF containers): read Orientation directly out
    // of IFD0 rather than handing the whole buffer to `exif::Reader` — see
    // `tiff_ifd0_orientation`. JPEG/PNG/WebP/HEIF keep the general-purpose reader.
    if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        return tiff_ifd0_orientation(bytes);
    }
    let exif = exif::Reader::new()
        .read_from_container(&mut std::io::Cursor::new(bytes))
        .ok()?;
    let field = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?;
    field.value.get_uint(0)
}

/// True if `bytes` is one of the containers `exif::Reader` can read (JPEG, TIFF,
/// PNG, WebP, HEIF/HEIC/AVIF) — the only formats that can carry an EXIF orientation.
pub(in super::super) fn has_exif_container(b: &[u8]) -> bool {
    b.len() >= 12
        && (b.starts_with(&[0xFF, 0xD8])                       // JPEG
            || b.starts_with(b"II*\0")                         // TIFF little-endian
            || b.starts_with(b"MM\0*")                         // TIFF big-endian
            || b.starts_with(&[0x89, b'P', b'N', b'G'])        // PNG (eXIf chunk)
            || (b.starts_with(b"RIFF") && &b[8..12] == b"WEBP") // WebP
            || &b[4..8] == b"ftyp") // ISOBMFF: HEIF/HEIC/AVIF
}
