//! Kindle / Mobipocket (MOBI / AZW / AZW3) cover extraction — hand-parsed in
//! pure Rust (deliberately NOT libmobi, which is LGPL and would impose relink
//! obligations on this MIT/Apache cdylib).
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
    // PalmDB record-info table: 8 bytes/record at offset 78; first u32 = data offset.
    let rec_off = |n: usize| -> Option<usize> { be32(bytes, 78 + n * 8).map(|v| v as usize) };
    let rec = |n: usize| -> Option<&[u8]> {
        let start = rec_off(n)?;
        let end = if n + 1 < rec_count { rec_off(n + 1)? } else { bytes.len() };
        if end < start {
            return None;
        }
        bytes.get(start..end)
    };

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

    // First image index. Calibre reads it from record0[108:112] (its canonical,
    // battle-tested offset); we trust that value when it actually lands on an
    // image record, else derive it from the first image we found.
    let image_base = be32(rec0, 108)
        .map(|v| v as usize)
        .filter(|&b| b != 0 && b < rec_count && rec(b).map(is_image).unwrap_or(false))
        .or_else(|| images.first().map(|&(i, _)| i));

    if let (Some(base), Some(mobi_len)) = (image_base, mobi_header_len(rec0)) {
        // EXTH CoverOffset (201) then ThumbOffset (202): cover = record(base + off).
        // EXTH is detected by its magic at record0[16 + mobi_header_len] (the
        // exth_flags bit at rec0[128] is what Calibre reads, but the magic is a
        // more robust gate). cover = record(image_base + offset), per Calibre.
        let exth_start = 16usize.saturating_add(mobi_len);
        for tag in [201u32, 202] {
            if let Some(off) = exth_u32(rec0, exth_start, tag) {
                if off != u32::MAX {
                    if let Some(idx) = base.checked_add(off as usize) {
                        if let Some(data) = rec(idx) {
                            if is_image(data) && data.len() as u64 <= super::MAX_COVER {
                                return Some(data.to_vec());
                            }
                        }
                    }
                }
            }
        }
        // No usable EXTH cover: Calibre falls back to the first image (image_base).
        if let Some(data) = rec(base) {
            if is_image(data) && data.len() as u64 <= super::MAX_COVER {
                return Some(data.to_vec());
            }
        }
    }

    // Last resort: the largest image record (avoids tiny publisher logos when the
    // base is unusable).
    let (idx, size) = images.iter().copied().max_by_key(|&(_, sz)| sz)?;
    (size as u64 <= super::MAX_COVER).then(|| rec(idx).map(<[u8]>::to_vec)).flatten()
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
    d.starts_with(&[0xFF, 0xD8, 0xFF]) // JPEG
        || d.starts_with(&[0x89, b'P', b'N', b'G']) // PNG
        || d.starts_with(b"GIF8") // GIF
        || d.starts_with(b"BM") // BMP
        || (d.len() >= 12 && &d[0..4] == b"RIFF" && &d[8..12] == b"WEBP") // WebP
}

fn be16(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

fn be32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(off..off + 4)?.try_into().ok()?))
}
