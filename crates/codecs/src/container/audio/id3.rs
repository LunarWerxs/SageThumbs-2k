//! DSD (`.dsf`) album art, via the trailing ID3v2 tag its header points at, plus
//! the ID3v2 `APIC` frame reader that pulls the front cover out of one.

use super::*;

/// Cap on the trailing ID3v2 tag a `.dsf` file's metadata pointer can claim, same
/// budget as the sibling ASF/APEv2 tag caps (`asf::MAX_ASF_HEADER`, `ape::MAX_APE_TAG`).
const MAX_DSF_ID3_TAG: u64 = crate::container::MAX_COVER + 1024 * 1024;

/// DSD (`.dsf`) album art. lofty 0.22 has no DSF reader, so — like the hand-rolled
/// ASF and APEv2 paths above — we parse it directly: the `DSD ` header chunk holds a
/// pointer to a trailing **ID3v2** tag (the same tag MP3 puts at the *front*), and we
/// pull the front-cover `APIC` frame out of it. Non-DSF input bails on the magic.
pub(super) fn dsf_cover<R: Read + Seek>(reader: &mut R) -> Option<Vec<u8>> {
    reader.seek(SeekFrom::Start(0)).ok()?;
    let mut hdr = [0u8; 28];
    reader.read_exact(&mut hdr).ok()?;
    if &hdr[0..4] != b"DSD " {
        return None; // not DSD — let the lofty path try it
    }
    // Bytes 20..28: file offset of the metadata (ID3v2) chunk; 0 == no metadata.
    let meta_ptr = le64(&hdr, 20)?;
    if meta_ptr == 0 {
        return None;
    }
    reader.seek(SeekFrom::Start(meta_ptr)).ok()?;
    let mut id3 = [0u8; 10];
    reader.read_exact(&mut id3).ok()?;
    if &id3[0..3] != b"ID3" {
        return None;
    }
    let major = id3[3];
    // The ID3v2 tag size is always synchsafe. Cap the read so a bogus size can't
    // force a huge allocation; a real cover tag is comfortably under this.
    let tag_len = (id3_synchsafe(&id3[6..10])? as usize).min(MAX_DSF_ID3_TAG as usize);
    let mut body = vec![0u8; tag_len];
    reader.read_exact(&mut body).ok()?;
    id3v2_front_cover(&body, major)
}

/// Scan ID3v2 frames for the best `APIC` picture: the front cover, and the LARGEST one
/// when a tag carries several.
///
/// It used to return the first type-3 frame and otherwise the first frame of any type.
/// Both halves lose to junk art — type 1 IS a "32x32 file icon" and taggers do write one
/// — so ranking by [`super::id3_pic_rank`] and then by size is the whole fix. See the
/// note on `audio::lofty_cover` for the case that found it.
fn id3v2_front_cover(body: &[u8], major: u8) -> Option<Vec<u8>> {
    let mut pos = 0usize;
    // (rank, size) of the best picture so far; strictly better replaces it, so a tie
    // keeps the earlier frame exactly as the old first-wins behaviour did.
    let mut best: Option<(u8, Vec<u8>)> = None;
    while pos + 10 <= body.len() {
        match scan_frame(body, pos, major, &mut best) {
            // Malformed frame-size field: abort with no cover, exactly as the old `?`s did.
            None => return None,
            // Padding or an over-long frame: stop, keeping the best found so far.
            Some(None) => break,
            Some(Some(next)) => pos = next,
        }
    }
    best.map(|(_, img)| img)
}

/// Read the one ID3v2 frame at `pos`, updating `best` when it holds a better `APIC` cover.
/// Returns `Some(next_pos)` to keep walking, `Some(None)` on padding or an over-long frame,
/// and `None` when the frame-size field is malformed (the caller then aborts, as `?` did).
fn scan_frame(
    body: &[u8],
    pos: usize,
    major: u8,
    best: &mut Option<(u8, Vec<u8>)>,
) -> Option<Option<usize>> {
    let id = &body[pos..pos + 4];
    if id == [0, 0, 0, 0] {
        return Some(None); // padding region — no more frames
    }
    let size = id3_frame_size(&body[pos + 4..pos + 8], major)? as usize;
    let start = pos + 10;
    let end = start.checked_add(size)?;
    if end > body.len() {
        return Some(None);
    }
    if id == b"APIC" {
        if let Some((ptype, img)) = parse_apic(&body[start..end]) {
            let rank = super::id3_pic_rank(ptype);
            if beats_best(best.as_ref(), rank, img.len()) {
                *best = Some((rank, img));
            }
        }
    }
    Some(Some(end))
}

/// Does a picture of `rank` and `len` bytes replace the best so far? Lower rank always wins;
/// inside one rank, the bigger image does; a tie keeps the earlier frame.
fn beats_best(best: Option<&(u8, Vec<u8>)>, rank: u8, len: usize) -> bool {
    match best {
        None => true,
        Some((r, cur)) => rank < *r || (rank == *r && len > cur.len()),
    }
}

/// Parse one `APIC` frame body: `encoding(u8), mime(latin1\0), pic_type(u8),
/// description(\0 — 2 bytes for UTF-16), image[…]`. Returns `(pic_type, image)` when
/// the trailing bytes are a size-bounded raster we can decode.
fn parse_apic(d: &[u8]) -> Option<(u8, Vec<u8>)> {
    let enc = *d.first()?;
    let p = skip_nul(d, 1, false)?; // MIME type (latin1, NUL-terminated)
    let ptype = *d.get(p)?;
    let p = p + 1;
    // Description, NUL-terminated. UTF-16 (enc 1/2) uses a 2-byte terminator.
    let p = skip_nul(d, p, enc == 1 || enc == 2)?;
    let img = d.get(p..)?;
    (crate::container::looks_like_raster(img) && img.len() as u64 <= crate::container::MAX_COVER)
        .then(|| (ptype, img.to_vec()))
}

/// Advance `p` past the NUL-terminated string starting at `p` in `d`, returning the index
/// just after its terminator (a 2-byte terminator when `wide`, for UTF-16 text).
fn skip_nul(d: &[u8], mut p: usize, wide: bool) -> Option<usize> {
    if wide {
        loop {
            let pair = d.get(p..p + 2)?;
            p += 2;
            if pair == [0, 0] {
                break;
            }
        }
    } else {
        while *d.get(p)? != 0 {
            p += 1;
        }
        p += 1;
    }
    Some(p)
}

/// An ID3v2 frame's size field: synchsafe in ID3v2.4, plain big-endian in 2.3 (and 2.2 on
/// a 2.3 header).
fn id3_frame_size(sz: &[u8], major: u8) -> Option<u32> {
    if major >= 4 {
        id3_synchsafe(sz)
    } else {
        Some(u32::from_be_bytes([sz[0], sz[1], sz[2], sz[3]]))
    }
}

/// Decode a 4-byte ID3v2 synchsafe integer (the high bit of each byte is zero).
fn id3_synchsafe(b: &[u8]) -> Option<u32> {
    let b = b.get(0..4)?;
    Some(
        ((b[0] as u32 & 0x7f) << 21)
            | ((b[1] as u32 & 0x7f) << 14)
            | ((b[2] as u32 & 0x7f) << 7)
            | (b[3] as u32 & 0x7f),
    )
}

/// Direct fuzz entry point into the ID3v2 frame walk, for `container::fuzzseed`. Going
/// through [`dsf_cover`]'s trailing-pointer + tag-size gate only reaches this once a
/// mutation leaves that framing intact — see `container::apk_fuzzapi` for the same argument
/// made about zip-wrapped parsers. Calls the existing parser directly and changes no
/// behavior. Major version 3 (plain big-endian frame sizes) is fixed here; the seed this
/// drives is built the same way.
#[cfg(test)]
#[doc(hidden)]
pub(crate) mod fuzzapi {
    use super::*;

    /// The frame walk on raw frame bytes, robustness only (result discarded).
    pub(crate) fn front_cover(body: &[u8]) {
        let _ = id3v2_front_cover(body, 3);
    }

    // ── seed self-check ──────────────────────────────────────────────────────────────
    // Returns its result so `container::fuzzseed::tests::every_seed_reaches_its_parser` can
    // assert the seed actually gets past the front door.
    pub(crate) fn front_cover_result(body: &[u8]) -> Option<Vec<u8>> {
        id3v2_front_cover(body, 3)
    }
}
