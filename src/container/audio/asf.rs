//! A hand-rolled ASF/WMA header parser: cover art (`WM/Picture`) and the text
//! tags. lofty has NO ASF support at all - its `FileType` enum has no Wma/Asf
//! variant - so a WMP/foobar-tagged .wma never reaches a picture through it.

use super::*;

/// ASF object GUIDs, in on-disk byte order (Data1/2/3 little-endian). The
/// Extended Content Description Object is a direct child of the Header Object,
/// but the Metadata / Metadata Library Objects are nested one level deeper inside
/// the Header Extension Object — so we have to descend into that too.
pub(super) const ASF_HEADER_GUID: [u8; 16] = [
    0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C,
];
// Header Extension Object = GUID 5FBF03B5-A92E-11CF-8EE3-00C00C205365 (verified against
// the ASF spec AND real ffmpeg/mutagen/WMP files — do NOT "correct" the Data1/Data2 bytes).
pub(super) const ASF_HDR_EXT_GUID: [u8; 16] = [
    0xB5, 0x03, 0xBF, 0x5F, 0x2E, 0xA9, 0xCF, 0x11, 0x8E, 0xE3, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65,
];
pub(super) const ASF_ECD_GUID: [u8; 16] = [
    0x40, 0xA4, 0xD0, 0xD2, 0x07, 0xE3, 0xD2, 0x11, 0x97, 0xF0, 0x00, 0xA0, 0xC9, 0x5E, 0xA8, 0x50,
];
pub(super) const ASF_MDLIB_GUID: [u8; 16] = [
    0x94, 0x1C, 0x23, 0x44, 0x98, 0x94, 0xD1, 0x49, 0xA1, 0x41, 0x1D, 0x13, 0x4E, 0x45, 0x70, 0x54,
];
pub(super) const ASF_META_GUID: [u8; 16] = [
    0xEA, 0xCB, 0xF8, 0xC5, 0xAF, 0x5B, 0x77, 0x48, 0x84, 0x67, 0xAA, 0x8C, 0x44, 0xFA, 0x4C, 0xCA,
];
/// Content Description Object — the fixed Title/Author/Copyright/Description/Rating
/// fields. Same GUID as the Header Object except the first byte (0x33 vs 0x30).
pub(super) const ASF_CONTENT_DESC_GUID: [u8; 16] = [
    0x33, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C,
];
/// File Properties Object — GUID 8CABDCA1-A947-11CF-8EE4-00C00C205365. Carries Play Duration
/// (100-ns) and Maximum Bitrate (bps), which lofty can't read for ASF (no ASF support at all).
const ASF_FILE_PROPS_GUID: [u8; 16] = [
    0xA1, 0xDC, 0xAB, 0x8C, 0x47, 0xA9, 0xCF, 0x11, 0x8E, 0xE4, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65,
];

/// Max ASF Header Object we'll read into memory (cover + slack; bomb guard). The
/// art lives inside this header, so we never touch the (huge) media body.
const MAX_ASF_HEADER: u64 = crate::container::MAX_COVER + 1024 * 1024;

/// Read the ASF Header Object body (everything after the 30-byte object header)
/// into memory, bounded by `MAX_ASF_HEADER`. `None` for non-ASF input. The album
/// art AND all tags live in this header, so we never read the (huge) media body.
fn asf_header_buf<R: Read + Seek>(r: &mut R) -> Option<Vec<u8>> {
    let len = r.seek(SeekFrom::End(0)).ok()?;
    if len < 30 {
        return None;
    }
    r.seek(SeekFrom::Start(0)).ok()?;
    // Header Object: GUID(16) + size(8) + object-count(4) + reserved(2) = 30 bytes.
    let mut head = [0u8; 30];
    r.read_exact(&mut head).ok()?;
    if head[0..16] != ASF_HEADER_GUID {
        return None;
    }
    let obj_size = le64(&head, 16)?;
    let end = obj_size.min(len).min(MAX_ASF_HEADER);
    if end <= 30 {
        return None;
    }
    let mut buf = vec![0u8; (end - 30) as usize];
    r.read_exact(&mut buf).ok()?;
    Some(buf)
}

/// Best album art from an ASF/WMA `WM/Picture` attribute — the front cover, and the
/// LARGEST one when a file carries several. `None` for non-ASF input or no picture.
///
/// `WM/Picture` carries the ID3 picture-type byte verbatim, so this shares
/// [`super::id3_pic_rank`] with the ID3 reader; type 1/2 are file icons and must never
/// beat the sleeve. See `audio::lofty_cover` for the case that found this.
pub(super) fn asf_cover<R: Read + Seek>(r: &mut R) -> Option<Vec<u8>> {
    let buf = asf_header_buf(r)?;
    let mut pics: Vec<(u8, Vec<u8>)> = Vec::new();
    walk_objects(&buf, 0, &mut |guid, payload| {
        collect_pictures(guid, payload, &mut pics)
    });
    // `min_by_key` keeps the first of a tie, so a single-picture file is unchanged.
    pics.iter()
        .min_by_key(|(t, img)| (super::id3_pic_rank(*t), std::cmp::Reverse(img.len())))
        .map(|(_, img)| img.clone())
}

/// Artist / album / title / track read from an ASF/WMA file's tag objects.
#[derive(Default)]
pub(crate) struct AsfTags {
    pub artist: Option<String>,
    pub album: Option<String>,
    pub title: Option<String>,
    pub track: Option<u32>,
    pub genre: Option<String>,
    pub year: Option<u32>,
    /// Playback length in ms (0 = unknown), from the File Properties Object.
    pub duration_ms: u64,
    /// Overall bitrate in kbps (0 = unknown), from the File Properties Object's Maximum Bitrate.
    pub bitrate_kbps: u32,
}

/// Pull artist/album/title/track from an ASF/WMA file (the Content Description
/// Object's fixed fields + the WM/* string attributes). lofty can't read ASF at
/// all, so without this the "Rename/Sort by audio tag" verbs do nothing for `.wma`.
/// `None` for non-ASF input → callers fall back to the lofty tag path.
pub(crate) fn asf_tags<R: Read + Seek>(r: &mut R) -> Option<AsfTags> {
    let buf = asf_header_buf(r)?;
    let mut tags = AsfTags::default();
    walk_objects(&buf, 0, &mut |guid, payload| {
        collect_tags(guid, payload, &mut tags)
    });
    Some(tags)
}

/// Walk a run of concatenated ASF objects (`GUID(16) + size(8) + payload`), calling
/// `visit(guid, payload)` for each leaf object and descending one level into the
/// Header Extension Object (which nests the Metadata / Metadata Library Objects).
/// The 4096-object and depth-2 caps guard a malformed/looping graph; we stop on the
/// first truncated/over-long object.
fn walk_objects(buf: &[u8], depth: u8, visit: &mut impl FnMut(&[u8], &[u8])) {
    if depth > 2 {
        return;
    }
    let mut p = 0usize;
    for _ in 0..4096 {
        if p + 24 > buf.len() {
            break;
        }
        let size = match le64(buf, p + 16) {
            Some(s) => s as usize,
            None => break,
        };
        let obj_end = match p.checked_add(size) {
            Some(e) if size >= 24 && e <= buf.len() => e,
            _ => break,
        };
        let guid = &buf[p..p + 16];
        let payload = &buf[p + 24..obj_end];
        if guid == ASF_HDR_EXT_GUID {
            // Header Extension Object payload: reserved GUID(16) + reserved u16(2) +
            // data-size u32(4), then the nested objects. Recurse into them.
            if let Some(nested) = payload.get(22..) {
                walk_objects(nested, depth + 1, visit);
            }
        } else {
            visit(guid, payload);
        }
        p = obj_end;
    }
}

/// Collect every `WM/Picture` (byte-array) attribute from an Extended Content
/// Description / Metadata / Metadata Library Object into `out`.
fn collect_pictures(guid: &[u8], payload: &[u8], out: &mut Vec<(u8, Vec<u8>)>) {
    let take = |name: &[u8], dtype: u16, val: &[u8]| {
        if dtype == 1 && name_eq(name, b"WM/Picture") {
            if let Some(pic) = parse_wm_picture(val) {
                out.push(pic);
            }
        }
    };
    if guid == ASF_ECD_GUID {
        ecd_attrs(payload, take);
    } else if guid == ASF_MDLIB_GUID || guid == ASF_META_GUID {
        mdlib_attrs(payload, take);
    }
}

/// Collect artist/album/title/track from a tag object into `tags`.
fn collect_tags(guid: &[u8], payload: &[u8], tags: &mut AsfTags) {
    if guid == ASF_CONTENT_DESC_GUID {
        cd_text(payload, tags);
    } else if guid == ASF_ECD_GUID {
        ecd_attrs(payload, |name, dtype, val| {
            apply_text_attr(name, dtype, val, tags)
        });
    } else if guid == ASF_MDLIB_GUID || guid == ASF_META_GUID {
        mdlib_attrs(payload, |name, dtype, val| {
            apply_text_attr(name, dtype, val, tags)
        });
    } else if guid == ASF_FILE_PROPS_GUID {
        file_props(payload, tags);
    }
}

/// Read Play Duration + Maximum Bitrate from the File Properties Object body. Offsets (after the
/// 24-byte object header `walk_objects` already stripped): play_duration u64 @40 (100-ns units,
/// INCLUDES the preroll), preroll u64 @56 (ms), max_bitrate u32 @76 (bits/sec). Bounds-checked by
/// `le64`/`le32` (`?` bails on a short body).
fn file_props(body: &[u8], tags: &mut AsfTags) -> Option<()> {
    let play_ms = le64(body, 40)? / 10_000;
    let preroll_ms = le64(body, 56)?;
    tags.duration_ms = play_ms.saturating_sub(preroll_ms);
    tags.bitrate_kbps = le32(body, 76)? / 1000;
    Some(())
}

/// Extended Content Description Object body: `count(u16)` then descriptors of
/// `name-len(u16), name, value-type(u16), value-len(u16), value`. Yields
/// `(name, value-type, value)` for each. Stops at the first malformed entry.
fn ecd_attrs(body: &[u8], mut visit: impl FnMut(&[u8], u16, &[u8])) -> Option<()> {
    let count = le16(body, 0)?;
    let mut p = 2usize;
    for _ in 0..count {
        let name_len = le16(body, p)? as usize;
        let ns = p.checked_add(2)?;
        let ne = ns.checked_add(name_len)?;
        let name = body.get(ns..ne)?;
        let vtype = le16(body, ne)?;
        let vlen = le16(body, ne.checked_add(2)?)? as usize;
        let vs = ne.checked_add(4)?;
        let ve = vs.checked_add(vlen)?;
        let val = body.get(vs..ve)?;
        visit(name, vtype, val);
        p = ve;
    }
    Some(())
}

/// Metadata / Metadata Library Object body: `count(u16)` then records of `lang(u16),
/// stream(u16), name-len(u16), data-type(u16), data-len(u32), name, data`. Yields
/// `(name, data-type, data)` for each (full-size album art + extended tags live here).
fn mdlib_attrs(body: &[u8], mut visit: impl FnMut(&[u8], u16, &[u8])) -> Option<()> {
    let count = le16(body, 0)?;
    let mut p = 2usize;
    for _ in 0..count {
        let name_len = le16(body, p.checked_add(4)?)? as usize;
        let dtype = le16(body, p.checked_add(6)?)?;
        let data_len = le32(body, p.checked_add(8)?)? as usize;
        let ns = p.checked_add(12)?;
        let ne = ns.checked_add(name_len)?;
        let name = body.get(ns..ne)?;
        let de = ne.checked_add(data_len)?;
        let data = body.get(ne..de)?;
        visit(name, dtype, data);
        p = de;
    }
    Some(())
}

/// Content Description Object: `title-len, author-len, copyright-len, description-len,
/// rating-len` (each u16) then those five UTF-16LE strings. We want title + author.
fn cd_text(body: &[u8], tags: &mut AsfTags) -> Option<()> {
    let title_len = le16(body, 0)? as usize;
    let author_len = le16(body, 2)? as usize;
    let title_end = 10usize.checked_add(title_len)?;
    let author_end = title_end.checked_add(author_len)?;
    if let Some(s) = utf16_string(body.get(10..title_end)?) {
        tags.title.get_or_insert(s);
    }
    if let Some(s) = utf16_string(body.get(title_end..author_end)?) {
        tags.artist.get_or_insert(s);
    }
    Some(())
}

/// Map one Extended-Content-Description / Metadata-Library attribute onto the tags
/// we care about (first value wins). Artist comes from `Author` only — the *track*
/// artist, matching lofty's `artist()` for every other format — NOT `WM/AlbumArtist`
/// (the album artist), which is a different field and would otherwise win on files
/// that store the ECD before the Content Description Object (the common layout).
fn apply_text_attr(name: &[u8], dtype: u16, value: &[u8], tags: &mut AsfTags) {
    match dtype {
        0 => apply_unicode_attr(name, value, tags),
        // DWORD: WM/Track is a zero-based integer
        3 if name_eq(name, b"WM/Track") && tags.track.is_none() => {
            tags.track = le32(value, 0).map(|n| n.saturating_add(1));
        }
        _ => {}
    }
}

/// One dtype-0 (Unicode string) attribute onto the tags we care about, first value wins.
fn apply_unicode_attr(name: &[u8], value: &[u8], tags: &mut AsfTags) {
    let Some(s) = utf16_string(value) else { return };
    if name_eq(name, b"WM/AlbumTitle") {
        tags.album.get_or_insert(s);
    } else if name_eq(name, b"Author") {
        tags.artist.get_or_insert(s);
    } else if name_eq(name, b"Title") {
        tags.title.get_or_insert(s);
    } else if name_eq(name, b"WM/TrackNumber") && tags.track.is_none() {
        tags.track = parse_track(&s);
    } else if name_eq(name, b"WM/Genre") {
        tags.genre.get_or_insert(s);
    } else if name_eq(name, b"WM/Year") && tags.year.is_none() {
        // WM/Year is a string ("2003"); keep only the leading 4-digit year.
        tags.year = s.get(..4).and_then(|y| y.parse().ok());
    }
}

/// Does the UTF-16LE attribute `name` equal the ASCII `want` (allowing one trailing NUL)?
pub(super) fn name_eq(name: &[u8], want: &[u8]) -> bool {
    if name.len() < want.len() * 2 {
        return false;
    }
    for (i, &c) in want.iter().enumerate() {
        if name[i * 2] != c || name[i * 2 + 1] != 0 {
            return false;
        }
    }
    matches!(&name[want.len() * 2..], [] | [0, 0])
}

/// Decode UTF-16LE bytes to a String, trimmed of trailing NULs/whitespace. `None`
/// if empty after trimming.
fn utf16_string(bytes: &[u8]) -> Option<String> {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let s = String::from_utf16_lossy(&units);
    let s = s.trim_end_matches('\0').trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Parse a track-number string ("5" or "5/12") into its leading integer.
fn parse_track(s: &str) -> Option<u32> {
    s.split(['/', ' ']).next()?.trim().parse().ok()
}

/// Parse a `WM/Picture` byte-array value: `type(u8), data-len(u32), mime(UTF-16\0),
/// description(UTF-16\0), image[data-len]`. Returns `(picture_type, image_bytes)` if
/// the image is a raster we can decode and within the size cap.
fn parse_wm_picture(v: &[u8]) -> Option<(u8, Vec<u8>)> {
    let ptype = *v.first()?;
    let data_len = le32(v, 1)? as usize;
    // Skip the two UTF-16LE NUL-terminated strings (MIME type, then description).
    let mut p = skip_utf16z(v, 5)?;
    p = skip_utf16z(v, p)?;
    let img = v.get(p..p.checked_add(data_len)?)?;
    (crate::container::looks_like_raster(img) && img.len() as u64 <= crate::container::MAX_COVER)
        .then(|| (ptype, img.to_vec()))
}

/// Advance past a UTF-16LE NUL-terminated string; returns the offset just after the
/// 2-byte `00 00` terminator, or `None` if there's none within bounds.
fn skip_utf16z(v: &[u8], mut p: usize) -> Option<usize> {
    loop {
        let pair = v.get(p..p + 2)?;
        p += 2;
        if pair == [0, 0] {
            return Some(p);
        }
    }
}
