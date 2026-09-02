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
        let id = &body[pos..pos + 4];
        if id == [0, 0, 0, 0] {
            break; // padding region — no more frames
        }
        // Frame size is synchsafe in ID3v2.4, plain big-endian in 2.3/2.2-on-2.3-header.
        let sz = &body[pos + 4..pos + 8];
        let size = if major >= 4 {
            id3_synchsafe(sz)?
        } else {
            u32::from_be_bytes([sz[0], sz[1], sz[2], sz[3]])
        } as usize;
        let start = pos + 10;
        let end = start.checked_add(size)?;
        if end > body.len() {
            break;
        }
        if id == b"APIC" {
            if let Some((ptype, img)) = parse_apic(&body[start..end]) {
                let rank = super::id3_pic_rank(ptype);
                let better = match &best {
                    None => true,
                    // Lower rank always wins; inside one rank, the bigger image does.
                    Some((r, cur)) => rank < *r || (rank == *r && img.len() > cur.len()),
                };
                if better {
                    best = Some((rank, img));
                }
            }
        }
        pos = end;
    }
    best.map(|(_, img)| img)
}

/// Parse one `APIC` frame body: `encoding(u8), mime(latin1\0), pic_type(u8),
/// description(\0 — 2 bytes for UTF-16), image[…]`. Returns `(pic_type, image)` when
/// the trailing bytes are a size-bounded raster we can decode.
fn parse_apic(d: &[u8]) -> Option<(u8, Vec<u8>)> {
    let enc = *d.first()?;
    let mut p = 1usize;
    while *d.get(p)? != 0 {
        p += 1; // MIME type (latin1, NUL-terminated)
    }
    p += 1;
    let ptype = *d.get(p)?;
    p += 1;
    // Description, NUL-terminated. UTF-16 (enc 1/2) uses a 2-byte terminator.
    if enc == 1 || enc == 2 {
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
    let img = d.get(p..)?;
    (crate::container::looks_like_raster(img) && img.len() as u64 <= crate::container::MAX_COVER)
        .then(|| (ptype, img.to_vec()))
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
