//! Kindle / Mobipocket (MOBI / AZW / AZW3) cover extraction — hand-parsed in
//! pure Rust (deliberately NOT libmobi, which is LGPL and would impose relink
//! obligations on this cdylib).
//!
//! Layout: PalmDB (record table) -> record 0 = PalmDOC header (16 B) + MOBI
//! header + EXTH. EXTH record 201 (CoverOffset) / 202 (ThumbOffset) give an
//! index relative to the MOBI header's first_image_index; that record IS a
//! complete JPEG/PNG/GIF. Every offset is checked against the byte slice.

pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    let rec_count = be16(bytes, 76)? as usize;
    if rec_count == 0 {
        return None;
    }
    let rec = |n: usize| record(bytes, rec_count, n);

    let rec0 = rec(0)?;
    // PalmDOC header: encryption type at offset 12 — only handle unencrypted books.
    if be16(rec0, 12)? != 0 {
        return None;
    }

    // All image-resource records (for the base derivation + last-resort fallback).
    let images: Vec<(usize, usize)> = (1..rec_count)
        .filter_map(|n| {
            let d = rec(n)?;
            is_image(d).then_some((n, d.len()))
        })
        .collect();

    pick_cover(rec0, rec_count, &images, &rec)
}

/// PalmDB record `n`: its data offset from the record-info table (8 bytes/record at offset
/// 78; first u32), sliced up to the next record or end of file, or None if unusable.
fn record(bytes: &[u8], rec_count: usize, n: usize) -> Option<&[u8]> {
    let start = be32(bytes, 78 + n * 8).map(|v| v as usize)?;
    let end = if n + 1 < rec_count {
        be32(bytes, 78 + (n + 1) * 8).map(|v| v as usize)?
    } else {
        bytes.len()
    };
    if end < start {
        return None;
    }
    bytes.get(start..end)
}

/// Pick the cover: EXTH CoverOffset/ThumbOffset at the derived image base, else the largest
/// image record (avoids tiny publisher logos when the base is unusable).
fn pick_cover<'a>(
    rec0: &'a [u8],
    rec_count: usize,
    images: &[(usize, usize)],
    rec: &dyn Fn(usize) -> Option<&'a [u8]>,
) -> Option<Vec<u8>> {
    // First image index. Calibre reads it from record0[108:112] (its canonical,
    // battle-tested offset); we trust that value when it actually lands on an
    // image record, else derive it from the first image we found.
    let image_base = be32(rec0, 108)
        .map(|v| v as usize)
        .filter(|&b| b != 0 && b < rec_count && rec(b).map(is_image).unwrap_or(false))
        .or_else(|| images.first().map(|&(i, _)| i));

    if let (Some(base), Some(mobi_len)) = (image_base, mobi_header_len(rec0)) {
        if let Some(cover) = cover_via_exth_or_base(rec0, base, mobi_len, rec) {
            return Some(cover);
        }
    }

    // Last resort: the largest image record (avoids tiny publisher logos when the
    // base is unusable).
    let (idx, size) = images.iter().copied().max_by_key(|&(_, sz)| sz)?;
    (size as u64 <= super::MAX_COVER)
        .then(|| rec(idx).map(<[u8]>::to_vec))
        .flatten()
}

/// EXTH CoverOffset (201) then ThumbOffset (202): cover = record(base + off). EXTH is
/// detected by its magic at record0[16 + mobi_header_len] (the exth_flags bit at
/// rec0[128] is what Calibre reads, but the magic is a more robust gate). Falls back to
/// the first image (`base` itself) when no usable EXTH cover exists, per Calibre.
fn cover_via_exth_or_base<'a>(
    rec0: &'a [u8],
    base: usize,
    mobi_len: usize,
    rec: &dyn Fn(usize) -> Option<&'a [u8]>,
) -> Option<Vec<u8>> {
    let exth_start = 16usize.saturating_add(mobi_len);
    for tag in [201u32, 202] {
        if let Some(cover) = exth_cover_at(rec0, exth_start, tag, base, rec) {
            return Some(cover);
        }
    }
    if let Some(data) = rec(base) {
        if is_image(data) && data.len() as u64 <= super::MAX_COVER {
            return Some(data.to_vec());
        }
    }
    None
}

/// Resolve one EXTH CoverOffset/ThumbOffset tag to a cover record, or None when the offset
/// is absent, `u32::MAX`, out of range, or not a viable image.
fn exth_cover_at<'a>(
    rec0: &'a [u8],
    exth_start: usize,
    tag: u32,
    base: usize,
    rec: &dyn Fn(usize) -> Option<&'a [u8]>,
) -> Option<Vec<u8>> {
    let off = exth_u32(rec0, exth_start, tag)?;
    if off == u32::MAX {
        return None;
    }
    let idx = base.checked_add(off as usize)?;
    let data = rec(idx)?;
    if is_image(data) && data.len() as u64 <= super::MAX_COVER {
        return Some(data.to_vec());
    }
    None
}

/// MOBI header length at record0[20:24], or None if there's no MOBI header.
fn mobi_header_len(rec0: &[u8]) -> Option<usize> {
    if rec0.get(16..20) == Some(b"MOBI") {
        be32(rec0, 20).map(|v| v as usize)
    } else {
        None
    }
}

/// The u32 payload of the first EXTH record of type `want`, or None.
fn exth_u32(rec0: &[u8], start: usize, want: u32) -> Option<u32> {
    if rec0.get(start..start + 4)? != b"EXTH" {
        return None;
    }
    let count = be32(rec0, start + 8)? as usize;
    let mut p = start + 12;
    for _ in 0..count.min(8192) {
        let typ = be32(rec0, p)?;
        let len = be32(rec0, p + 4)? as usize;
        if len < 8 {
            return None;
        }
        if typ == want {
            return be32(rec0, p + 8);
        }
        p = p.checked_add(len)?;
    }
    None
}

fn is_image(d: &[u8]) -> bool {
    super::looks_like_raster(d)
}

use super::util::{be16, be32};
