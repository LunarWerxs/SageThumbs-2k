//! APEv2 "Cover Art (Front/Back)" extraction. lofty reads the tag but not the
//! cover item, so the tag region at the end of the file is parsed by hand.

use super::*;

/// Max APEv2 tag region we'll read (cover + a little overhead; bomb guard).
const MAX_APE_TAG: u64 = crate::container::MAX_COVER + 1024 * 1024;

/// Extract a "Cover Art (Front/Back)" image from an APEv2 tag, which lives at the
/// end of the file (optionally before a 128-byte ID3v1 trailer). Reads only the
/// tag region. `None` if there's no APEv2 footer or no cover item.
pub(super) fn apev2_cover<R: Read + Seek>(r: &mut R) -> Option<Vec<u8>> {
    let len = r.seek(SeekFrom::End(0)).ok()?;
    // Footer is the last 32 bytes — or 32 before an ID3v1 ("TAG", 128 bytes).
    for back in [32u64, 160u64] {
        if len < back {
            continue;
        }
        r.seek(SeekFrom::Start(len - back)).ok()?;
        let mut footer = [0u8; 32];
        if r.read_exact(&mut footer).is_err() || &footer[0..8] != b"APETAGEX" {
            continue;
        }
        let tag_size = le32(&footer, 12)? as u64; // items + this 32-byte footer
        let count = le32(&footer, 16)? as usize;
        if !(32..=MAX_APE_TAG).contains(&tag_size) {
            continue;
        }
        let items_start = (len - back).checked_sub(tag_size - 32)?;
        r.seek(SeekFrom::Start(items_start)).ok()?;
        let mut buf = vec![0u8; (tag_size - 32) as usize];
        r.read_exact(&mut buf).ok()?;
        return parse_apev2_cover(&buf, count);
    }
    None
}

/// Read one `size(4) flags(4) key\0 value[size]` APEv2 item starting at `p`. Returns the item's
/// key, value, and the offset just past it, or `None` on a truncated/malformed item.
fn read_apev2_item(buf: &[u8], p: usize) -> Option<(&[u8], &[u8], usize)> {
    let vsize = le32(buf, p)? as usize;
    let mut p = p.checked_add(8)?; // size(4) + flags(4)
    let kstart = p;
    while p < buf.len() && buf[p] != 0 {
        p += 1;
    }
    let key = buf.get(kstart..p)?;
    p = p.checked_add(1)?; // skip the key's NUL
    let value = buf.get(p..p.checked_add(vsize)?)?;
    p += vsize;
    Some((key, value, p))
}

/// Front beats back: rank 0 for "Cover Art (Front)", 1 for "Cover Art (Back)", `None` for
/// anything else.
fn apev2_cover_rank(key: &[u8]) -> Option<u8> {
    if key.eq_ignore_ascii_case(b"cover art (front)") {
        Some(0)
    } else if key.eq_ignore_ascii_case(b"cover art (back)") {
        Some(1)
    } else {
        None
    }
}

/// Decode one cover item's `description\0imagedata` value and, if it validates, replace `best`
/// when it outranks (or, within the same rank, outsizes) the current candidate. A malformed or
/// oversized item is skipped, not fatal: the tag may still hold a good one further down.
fn consider_apev2_cover(best: &mut Option<(u8, Vec<u8>)>, rank: u8, value: &[u8]) {
    let Some(img) = value
        .iter()
        .position(|&b| b == 0)
        .and_then(|nul| value.get(nul + 1..))
    else {
        return;
    };
    if !crate::container::looks_like_raster(img) || img.len() as u64 > crate::container::MAX_COVER {
        return;
    }
    let better = match best {
        None => true,
        Some((r, cur)) => rank < *r || (rank == *r && img.len() > cur.len()),
    };
    if better {
        *best = Some((rank, img.to_vec()));
    }
}

/// Walk APEv2 items (`u32 size, u32 flags, key\0, value[size]`) for a cover-art
/// binary item; its value is `description\0imagedata`.
///
/// Front beats back, and inside one kind the bigger image wins. The old code returned
/// the FIRST item whose key was either, so a file listing "Cover Art (Back)" before the
/// front one thumbnailed as its back cover. Same defect the other three readers had; see
/// `audio::lofty_cover`.
fn parse_apev2_cover(buf: &[u8], count: usize) -> Option<Vec<u8>> {
    let mut best: Option<(u8, Vec<u8>)> = None;
    let mut p = 0usize;
    for _ in 0..count.min(512) {
        let (key, value, next_p) = read_apev2_item(buf, p)?;
        p = next_p;
        if let Some(rank) = apev2_cover_rank(key) {
            consider_apev2_cover(&mut best, rank, value);
        }
    }
    best.map(|(_, img)| img)
}

// ── ASF / WMA album art ──────────────────────────────────────────────────────
// A `.wma` stores cover art as a `WM/Picture` attribute inside the ASF Header
// Object — in the Extended Content Description Object (value length is u16, so
// only small covers) or the Metadata Library Object (data length is u32, the
// usual home for full-size art). Both carry the same `WM/Picture` byte-array
// struct. lofty can't even identify ASF, so we read just the (bounded) header
// object and pull the picture ourselves. Mirrors the APEv2 hand-roll above.
