//! AutoCAD `.dwg` embedded preview/thumbnail.
//!
//! Since R13 (AC1012) a DWG carries a "Preview/Thumbnail Image" section. The file
//! header holds a 4-byte LE seeker at offset `0x0D` pointing at that section, which
//! starts with a fixed 16-byte sentinel, then a record table: each record is a
//! 1-byte code + 4-byte LE absolute offset + 4-byte LE size. Code 2 = BMP/DIB,
//! 3 = WMF, 6 = PNG (newer saves). We pick the best, carve it, and (for a bare DIB)
//! prepend a `BITMAPFILEHEADER`. No CAD parsing, no SDK.
//!
//! Verified against real R13/2000/2004/2018 saves (DIB for older, PNG for 2018).
//! Files saved with the thumbnail preview disabled (`RASTERPREVIEW`/`THUMBSAVE`=0)
//! have a zero-size record → we return None. Runs under `panic = "abort"`: every
//! read is bounds-checked.

use super::util::{dib_to_bmp, le32};

const SENTINEL: [u8; 16] = [
    0x1F, 0x25, 0x6D, 0x07, 0xD4, 0x36, 0x28, 0x28, 0x9D, 0x57, 0xCA, 0x3F, 0x9D, 0x44, 0x10, 0x2B,
];

/// DWG version strings since R10 are "AC10xx" (R13=AC1012 … 2018=AC1032). The
/// preview section only exists R13+, gated by the sentinel check in `extract`.
pub fn looks_like_dwg(head: &[u8]) -> bool {
    head.starts_with(b"AC10")
}

/// How many leading bytes hold everything [`extract`] needs: the header seeker,
/// the preview section's record table, and the record payloads themselves. The
/// records carry ABSOLUTE offsets, so this is `max(off + size)` over the candidate
/// records (and the table's own end) — in real saves the preview sits right behind
/// the header, so this is typically a few hundred KB of a drawing that may be
/// hundreds of MB. Mirrors [`extract`]'s record filtering exactly, so it never
/// under-reads what `extract` will go on to slice.
///
/// None when there's no usable preview section (pre-R13, `RASTERPREVIEW=0`, or a
/// malformed/truncated header) — the caller then takes its normal whole-file path.
/// Generic over `Read + Seek` so the shell-IStream and by-path front-ends share it.
pub fn preview_prefix_len<R: std::io::Read + std::io::Seek>(r: &mut R) -> Option<u64> {
    use std::io::SeekFrom;
    let mut seeker = [0u8; 4];
    r.seek(SeekFrom::Start(0x0D)).ok()?;
    r.read_exact(&mut seeker).ok()?;
    let imgptr = u32::from_le_bytes(seeker) as u64;

    // Sentinel + overall-size u32 + record count, in one read.
    let mut hdr = [0u8; 21];
    r.seek(SeekFrom::Start(imgptr)).ok()?;
    r.read_exact(&mut hdr).ok()?;
    if hdr[..16] != SENTINEL {
        return None; // no preview section (or pre-R13)
    }
    let count = hdr[20] as usize;

    let mut table = vec![0u8; count.checked_mul(9)?];
    r.read_exact(&mut table).ok()?;
    // The table itself must be covered even if every record is filtered out.
    let mut end = imgptr.checked_add(21)?.checked_add(table.len() as u64)?;
    for rec in table.chunks_exact(9) {
        let off = u32::from_le_bytes(rec[1..5].try_into().ok()?) as u64;
        let size = u32::from_le_bytes(rec[5..9].try_into().ok()?) as u64;
        // Same filter as `extract`: zero-size and oversized records are skipped
        // there, so they must not inflate the prefix here either.
        if size == 0 || size > super::MAX_COVER || !matches!(rec[0], 2 | 3 | 6) {
            continue;
        }
        end = end.max(off.checked_add(size)?);
    }
    Some(end)
}

/// Extract the embedded preview (PNG as-is, DIB wrapped to BMP, or raw WMF), or None.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    let imgptr = le32(bytes, 0x0D)? as usize;
    if bytes.get(imgptr..imgptr.checked_add(16)?)? != SENTINEL {
        return None; // no preview section (or pre-R13)
    }
    let mut p = imgptr + 16;
    p = p.checked_add(4)?; // skip the overall image-data size (u32)
    let count = *bytes.get(p)?;
    p = p.checked_add(1)?;

    // Walk the records; prefer PNG (6) > DIB (2) > WMF (3).
    let (mut png, mut dib, mut wmf) = (None, None, None);
    for _ in 0..count {
        let code = *bytes.get(p)?;
        let off = le32(bytes, p.checked_add(1)?)? as usize;
        let size = le32(bytes, p.checked_add(5)?)? as usize;
        p = p.checked_add(9)?;
        if size == 0 || size as u64 > super::MAX_COVER {
            continue;
        }
        match code {
            6 => png = png.or(Some((off, size))),
            2 => dib = dib.or(Some((off, size))),
            3 => wmf = wmf.or(Some((off, size))),
            _ => {} // 1 = header/palette block, etc.
        }
    }

    if let Some((off, size)) = png {
        let p = bytes.get(off..off.checked_add(size)?)?;
        return super::util::decodable_image(p.to_vec());
    }
    if let Some((off, size)) = dib {
        let d = bytes.get(off..off.checked_add(size)?)?;
        return super::util::decodable_image(dib_to_bmp(d)?);
    }
    if let Some((off, size)) = wmf {
        let w = bytes.get(off..off.checked_add(size)?)?;
        return super::util::decodable_image(w.to_vec());
    }
    None
}

/// Test-only synthetic-DWG builder, shared with the head-preview fast-path tests
/// (re-exported as `container::dwg_testutil`). Lives outside `mod tests` so
/// sibling modules can reach it under cfg(test).
#[cfg(test)]
pub(crate) mod testutil {
    use super::SENTINEL;

    /// Minimal R2018-style DWG: header seeker at 0x0D -> preview section with one
    /// PNG record, then `tail` zero bytes standing in for the object database a
    /// real drawing is huge from. Returns the bytes plus the exact prefix length
    /// [`super::preview_prefix_len`] should report.
    ///
    /// The record table also carries THREE junk records ahead of the PNG, each
    /// disqualified by exactly ONE clause of the shared record filter (size == 0 /
    /// size > MAX_COVER / code not in {2,3,6}) and each pointing FAR past the real
    /// head — so if any single clause stops filtering, the computed prefix visibly
    /// inflates and the tests fail. cargo-mutants caught the original single-valid-
    /// record version passing with `||` mutated to `&&` (2026-07-21); these records
    /// are what make each clause individually load-bearing.
    pub(crate) fn synthetic_dwg(with_preview: bool, tail: usize) -> (Vec<u8>, usize) {
        const FAR: u32 = 200 << 20; // junk-record offset, way beyond any real head
        let png = {
            let mut b = Vec::new();
            image::DynamicImage::ImageRgba8(image::RgbaImage::new(2, 2))
                .write_to(&mut std::io::Cursor::new(&mut b), image::ImageFormat::Png)
                .unwrap();
            b
        };
        let mut f = b"AC1032\x00\x00\x00\x00\x00".to_vec();
        f.resize(13, 0);
        let imgptr = 64u32;
        f.extend_from_slice(&imgptr.to_le_bytes()); // seeker at 0x0D
        f.resize(imgptr as usize, 0);
        if !with_preview {
            // Seeker points at zeros: no sentinel, so no preview section.
            f.extend_from_slice(&[0u8; 32]);
            let head_len = f.len();
            f.extend_from_slice(&vec![0u8; tail]);
            return (f, head_len);
        }
        f.extend_from_slice(&SENTINEL);
        f.extend_from_slice(&0u32.to_le_bytes()); // overall size (ignored)
        f.push(4); // 3 junk records + the real PNG record
        // Fixed layout: table starts after sentinel(16) + size(4) + count(1),
        // each record is 9 bytes, and the PNG payload follows the table.
        let png_off = imgptr + 16 + 4 + 1 + 4 * 9;
        let recs: [(u8, u32, u32); 4] = [
            (6, FAR, 0),                                      // skipped ONLY by `size == 0`
            (6, FAR, (super::super::MAX_COVER as u32) + 1),   // ONLY by `size > MAX_COVER`
            (7, FAR, 100),                                    // ONLY by `code not in {2,3,6}`
            (6, png_off, png.len() as u32),                   // the real preview
        ];
        for (code, off, size) in recs {
            f.push(code);
            f.extend_from_slice(&off.to_le_bytes());
            f.extend_from_slice(&size.to_le_bytes());
        }
        debug_assert_eq!(f.len(), png_off as usize);
        f.extend_from_slice(&png);
        let head_len = f.len();
        f.extend_from_slice(&vec![0u8; tail]);
        (f, head_len)
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::synthetic_dwg;
    use super::*;

    #[test]
    fn preview_prefix_len_covers_exactly_the_record_payload() {
        use std::io::Cursor;
        // 4 MB of "object database" tail: the prefix stops right after the PNG
        // record's payload and still extracts.
        let (big, head_len) = synthetic_dwg(true, 4 << 20);
        let len = preview_prefix_len(&mut Cursor::new(&big)).expect("prefix len");
        assert_eq!(len, head_len as u64);
        assert!(extract(&big[..len as usize]).is_some());

        // No sentinel (RASTERPREVIEW=0 / pre-R13) -> no fast path, and `extract`
        // agrees there is nothing to find.
        let (bare, _) = synthetic_dwg(false, 1024);
        assert_eq!(preview_prefix_len(&mut Cursor::new(&bare)), None);
        assert!(extract(&bare).is_none());

        // Truncated before the record table -> None, not a panic.
        assert_eq!(preview_prefix_len(&mut Cursor::new(&big[..70])), None);
        assert_eq!(preview_prefix_len(&mut Cursor::new(b"not a dwg")), None);
    }

    /// Boundary pin for the record filter: a record of EXACTLY `MAX_COVER` bytes
    /// still qualifies (the filter is strictly-greater), so the prefix must cover
    /// it. Surfaced by cargo-mutants (`>` -> `>=` survived): the junk records in
    /// `synthetic_dwg` sit at MAX_COVER + 1, so nothing exercised the boundary.
    /// Math-only — the record payload never needs to exist, `preview_prefix_len`
    /// reads just the header and table.
    #[test]
    fn preview_prefix_len_keeps_a_record_at_exactly_max_cover() {
        use std::io::Cursor;
        let mut f = b"AC1032\x00\x00\x00\x00\x00".to_vec();
        f.resize(13, 0);
        let imgptr = 64u32;
        f.extend_from_slice(&imgptr.to_le_bytes());
        f.resize(imgptr as usize, 0);
        f.extend_from_slice(&SENTINEL);
        f.extend_from_slice(&0u32.to_le_bytes());
        f.push(1); // one record: DIB, exactly MAX_COVER bytes
        f.push(2);
        f.extend_from_slice(&4096u32.to_le_bytes());
        f.extend_from_slice(&(super::super::MAX_COVER as u32).to_le_bytes());

        let got = preview_prefix_len(&mut Cursor::new(&f));
        assert_eq!(got, Some(4096 + super::super::MAX_COVER));
    }

    #[test]
    fn no_sentinel_no_preview() {
        // "AC1032" header but a seeker pointing at non-sentinel bytes → None, no panic.
        let mut f = b"AC1032\x00\x00\x00\x00\x00".to_vec();
        f.resize(64, 0);
        f[0x0D..0x11].copy_from_slice(&20u32.to_le_bytes()); // seeker → offset 20 (zeros)
        assert!(extract(&f).is_none());
        assert!(looks_like_dwg(&f));
        assert!(!looks_like_dwg(b"not a dwg"));
    }

    #[test]
    fn extracts_png_record() {
        // Build a minimal DWG with a sentinel + one PNG record.
        let png = {
            let mut b = Vec::new();
            image::DynamicImage::ImageRgba8(image::RgbaImage::new(2, 2))
                .write_to(&mut std::io::Cursor::new(&mut b), image::ImageFormat::Png)
                .unwrap();
            b
        };
        let mut f = b"AC1032\x00\x00\x00\x00\x00".to_vec();
        f.resize(13, 0);
        let imgptr = 64u32;
        f.extend_from_slice(&imgptr.to_le_bytes()); // seeker at 0x0D
        f.resize(imgptr as usize, 0);
        f.extend_from_slice(&SENTINEL); // sentinel
        f.extend_from_slice(&0u32.to_le_bytes()); // overall size (ignored)
        f.push(1); // 1 record
        let png_off = (f.len() + 9) as u32; // record is 9 bytes, then the PNG
        f.push(6); // code = PNG
        f.extend_from_slice(&png_off.to_le_bytes());
        f.extend_from_slice(&(png.len() as u32).to_le_bytes());
        f.extend_from_slice(&png);

        let got = extract(&f).expect("should extract the PNG record");
        assert!(got.starts_with(&[0x89, b'P', b'N', b'G']));
    }
}
