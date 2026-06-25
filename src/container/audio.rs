//! Embedded album / cover art from audio files (MP3, FLAC, Ogg/Opus/Speex,
//! MP4/M4A, WMA, APE, WavPack, Musepack, WAV, AIFF) via the `lofty` crate.
//! Windows 11 doesn't thumbnail several of these at all (Ogg/Opus/APE/…), so we
//! pull the front-cover picture (or the first one) and hand its bytes to the
//! normal image tiers — same flow as an ebook cover.
//!
//! Two formats can't ride lofty for art and get hand-rolled extractors here:
//! APEv2 "Cover Art (Front)" (lofty reads the tag but not the cover item), and
//! ASF/WMA — lofty has NO ASF support at all (its `FileType` enum has no Wma/Asf
//! variant), so a real WMP/foobar-tagged `.wma` never reaches a picture via lofty.
//! `asf_cover` parses the `WM/Picture` attribute out of the ASF header directly.
//!
//! `extract_reader` takes a seekable reader so the thumbnail provider can hand us
//! the shell's IStream directly: lofty seeks to the metadata/art and reads only
//! that, never the whole (possibly multi-gigabyte audiobook) file.

use std::io::{Cursor, Read, Seek, SeekFrom};

use lofty::file::TaggedFileExt;
use lofty::picture::PictureType;
use lofty::probe::Probe;

use super::util::{le16, le32, le64};

/// Album art from a byte slice (used by the generic cover path / examples).
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    extract_reader(Cursor::new(bytes))
}

/// Album art from any seekable reader. lofty parses tags by seeking, so a huge
/// file costs only the reads needed to reach the picture block. Failing an
/// embedded cover, raw-PCM WAV/AIFF get a drawn waveform (see `waveform`); every
/// other format with no art returns `None` → the shell shows the default icon.
pub fn extract_reader<R: Read + Seek>(mut reader: R) -> Option<Vec<u8>> {
    // APEv2 cover art FIRST: lofty reads the APEv2 tag but does NOT expose its
    // "Cover Art (Front)" item as a picture, so Musepack (.mpc, APEv2-only for art)
    // — and any APEv2-cover WavPack/Monkey's-Audio — would otherwise return nothing.
    // This reads only the tag region at the file end (memory-light), then seeks back
    // for lofty. Absent on non-APEv2 files (fast footer reject) → lofty path runs.
    if let Some(cover) = apev2_cover(&mut reader) {
        return Some(cover);
    }
    // ASF/WMA SECOND: lofty can't identify ASF at all, so it would just error out
    // below. Hand-parse the `WM/Picture` attribute. Non-ASF input bails immediately
    // (GUID mismatch) → the lofty path runs as before.
    if let Some(cover) = asf_cover(&mut reader) {
        return Some(cover);
    }
    // DSD (.dsf) THIRD: lofty 0.22 has no DSF reader either, so hand-parse the DSD
    // header's pointer to its trailing ID3v2 tag and pull the cover out. Non-DSF
    // input bails on the magic → the lofty path runs as before.
    if let Some(cover) = dsf_cover(&mut reader) {
        return Some(cover);
    }
    // lofty for every other tagged format. Borrowed (`&mut`) so we keep ownership
    // of the reader for the waveform fallback below.
    if let Some(cover) = lofty_cover(&mut reader) {
        return Some(cover);
    }
    // No embedded art: draw a waveform for the raw-PCM families (WAV/AIFF). A
    // recognizable shape beats a blank icon; `None` for anything else.
    reader.seek(SeekFrom::Start(0)).ok()?;
    super::waveform::render_from_reader(&mut reader)
}

/// Front-cover (or first) album art via lofty's tag reader. `None` if the format
/// is unidentified, untagged, or carries no picture.
fn lofty_cover(reader: &mut dyn super::ReadSeek) -> Option<Vec<u8>> {
    reader.seek(SeekFrom::Start(0)).ok()?;
    let tagged = Probe::new(reader).guess_file_type().ok()?.read().ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let pics = tag.pictures();
    let pic = pics
        .iter()
        .find(|p| p.pic_type() == PictureType::CoverFront)
        .or_else(|| pics.first())?;
    let data = pic.data();
    // The art may itself be WebP/AVIF/JXL; the downstream image tiers decode what
    // they can and fall back to the default icon otherwise — we just bound size.
    (!data.is_empty() && data.len() as u64 <= super::MAX_COVER).then(|| data.to_vec())
}

/// Max APEv2 tag region we'll read (cover + a little overhead; bomb guard).
const MAX_APE_TAG: u64 = super::MAX_COVER + 1024 * 1024;

/// Extract a "Cover Art (Front/Back)" image from an APEv2 tag, which lives at the
/// end of the file (optionally before a 128-byte ID3v1 trailer). Reads only the
/// tag region. `None` if there's no APEv2 footer or no cover item.
fn apev2_cover<R: Read + Seek>(r: &mut R) -> Option<Vec<u8>> {
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

/// Walk APEv2 items (`u32 size, u32 flags, key\0, value[size]`) for a cover-art
/// binary item; its value is `description\0imagedata`.
fn parse_apev2_cover(buf: &[u8], count: usize) -> Option<Vec<u8>> {
    let mut p = 0usize;
    for _ in 0..count.min(512) {
        let vsize = le32(buf, p)? as usize;
        p = p.checked_add(8)?; // size(4) + flags(4)
        let kstart = p;
        while p < buf.len() && buf[p] != 0 {
            p += 1;
        }
        let key = buf.get(kstart..p)?;
        p = p.checked_add(1)?; // skip the key's NUL
        let value = buf.get(p..p.checked_add(vsize)?)?;
        p += vsize;
        if key.eq_ignore_ascii_case(b"cover art (front)") || key.eq_ignore_ascii_case(b"cover art (back)") {
            let nul = value.iter().position(|&b| b == 0)?; // description\0image
            let img = value.get(nul + 1..)?;
            if super::looks_like_raster(img) && img.len() as u64 <= super::MAX_COVER {
                return Some(img.to_vec());
            }
        }
    }
    None
}

// ── ASF / WMA album art ──────────────────────────────────────────────────────
// A `.wma` stores cover art as a `WM/Picture` attribute inside the ASF Header
// Object — in the Extended Content Description Object (value length is u16, so
// only small covers) or the Metadata Library Object (data length is u32, the
// usual home for full-size art). Both carry the same `WM/Picture` byte-array
// struct. lofty can't even identify ASF, so we read just the (bounded) header
// object and pull the picture ourselves. Mirrors the APEv2 hand-roll above.

/// ASF object GUIDs, in on-disk byte order (Data1/2/3 little-endian). The
/// Extended Content Description Object is a direct child of the Header Object,
/// but the Metadata / Metadata Library Objects are nested one level deeper inside
/// the Header Extension Object — so we have to descend into that too.
const ASF_HEADER_GUID: [u8; 16] =
    [0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C];
// Header Extension Object = GUID 5FBF03B5-A92E-11CF-8EE3-00C00C205365 (verified against
// the ASF spec AND real ffmpeg/mutagen/WMP files — do NOT "correct" the Data1/Data2 bytes).
const ASF_HDR_EXT_GUID: [u8; 16] =
    [0xB5, 0x03, 0xBF, 0x5F, 0x2E, 0xA9, 0xCF, 0x11, 0x8E, 0xE3, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65];
const ASF_ECD_GUID: [u8; 16] =
    [0x40, 0xA4, 0xD0, 0xD2, 0x07, 0xE3, 0xD2, 0x11, 0x97, 0xF0, 0x00, 0xA0, 0xC9, 0x5E, 0xA8, 0x50];
const ASF_MDLIB_GUID: [u8; 16] =
    [0x94, 0x1C, 0x23, 0x44, 0x98, 0x94, 0xD1, 0x49, 0xA1, 0x41, 0x1D, 0x13, 0x4E, 0x45, 0x70, 0x54];
const ASF_META_GUID: [u8; 16] =
    [0xEA, 0xCB, 0xF8, 0xC5, 0xAF, 0x5B, 0x77, 0x48, 0x84, 0x67, 0xAA, 0x8C, 0x44, 0xFA, 0x4C, 0xCA];
/// Content Description Object — the fixed Title/Author/Copyright/Description/Rating
/// fields. Same GUID as the Header Object except the first byte (0x33 vs 0x30).
const ASF_CONTENT_DESC_GUID: [u8; 16] =
    [0x33, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C];
/// File Properties Object — GUID 8CABDCA1-A947-11CF-8EE4-00C00C205365. Carries Play Duration
/// (100-ns) and Maximum Bitrate (bps), which lofty can't read for ASF (no ASF support at all).
const ASF_FILE_PROPS_GUID: [u8; 16] =
    [0xA1, 0xDC, 0xAB, 0x8C, 0x47, 0xA9, 0xCF, 0x11, 0x8E, 0xE4, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65];

/// Max ASF Header Object we'll read into memory (cover + slack; bomb guard). The
/// art lives inside this header, so we never touch the (huge) media body.
const MAX_ASF_HEADER: u64 = super::MAX_COVER + 1024 * 1024;

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

/// Front-cover (or first) album art from an ASF/WMA `WM/Picture` attribute.
/// `None` for non-ASF input or no embedded picture.
fn asf_cover<R: Read + Seek>(r: &mut R) -> Option<Vec<u8>> {
    let buf = asf_header_buf(r)?;
    let mut pics: Vec<(u8, Vec<u8>)> = Vec::new();
    walk_objects(&buf, 0, &mut |guid, payload| collect_pictures(guid, payload, &mut pics));
    pics.iter()
        .find(|(t, _)| *t == 3)
        .or_else(|| pics.first())
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
    walk_objects(&buf, 0, &mut |guid, payload| collect_tags(guid, payload, &mut tags));
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
        ecd_attrs(payload, |name, dtype, val| apply_text_attr(name, dtype, val, tags));
    } else if guid == ASF_MDLIB_GUID || guid == ASF_META_GUID {
        mdlib_attrs(payload, |name, dtype, val| apply_text_attr(name, dtype, val, tags));
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
        0 => {
            // Unicode string
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
        // DWORD: WM/Track is a zero-based integer
        3 if name_eq(name, b"WM/Track") && tags.track.is_none() => {
            tags.track = le32(value, 0).map(|n| n.saturating_add(1));
        }
        _ => {}
    }
}

/// Does the UTF-16LE attribute `name` equal the ASCII `want` (allowing one trailing NUL)?
fn name_eq(name: &[u8], want: &[u8]) -> bool {
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
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
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
    (super::looks_like_raster(img) && img.len() as u64 <= super::MAX_COVER)
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

/// DSD (`.dsf`) album art. lofty 0.22 has no DSF reader, so — like the hand-rolled
/// ASF and APEv2 paths above — we parse it directly: the `DSD ` header chunk holds a
/// pointer to a trailing **ID3v2** tag (the same tag MP3 puts at the *front*), and we
/// pull the front-cover `APIC` frame out of it. Non-DSF input bails on the magic.
fn dsf_cover<R: Read + Seek>(reader: &mut R) -> Option<Vec<u8>> {
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
    let tag_len = (id3_synchsafe(&id3[6..10])? as usize).min(64 * 1024 * 1024);
    let mut body = vec![0u8; tag_len];
    reader.read_exact(&mut body).ok()?;
    id3v2_front_cover(&body, major)
}

/// Scan ID3v2 frames for an `APIC` picture, preferring the front cover (type 3).
fn id3v2_front_cover(body: &[u8], major: u8) -> Option<Vec<u8>> {
    let mut pos = 0usize;
    let mut fallback: Option<Vec<u8>> = None;
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
                if ptype == 3 {
                    return Some(img); // front cover — best match
                }
                fallback.get_or_insert(img);
            }
        }
        pos = end;
    }
    fallback
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
    (super::looks_like_raster(img) && img.len() as u64 <= super::MAX_COVER).then(|| (ptype, img.to_vec()))
}

/// Decode a 4-byte ID3v2 synchsafe integer (the high bit of each byte is zero).
fn id3_synchsafe(b: &[u8]) -> Option<u32> {
    let b = b.get(0..4)?;
    Some(((b[0] as u32 & 0x7f) << 21) | ((b[1] as u32 & 0x7f) << 14) | ((b[2] as u32 & 0x7f) << 7) | (b[3] as u32 & 0x7f))
}

/// Cheap magic sniff so we only run lofty on actual audio containers. (Cover art
/// in MP3 lives in ID3v2, which sits at the file start, so "ID3" covers MP3.)
pub fn looks_like_audio(b: &[u8]) -> bool {
    b.starts_with(b"ID3")                                       // MP3 (ID3v2)
        || b.starts_with(b"fLaC")                               // FLAC
        || b.starts_with(b"OggS")                               // Ogg: Vorbis/Opus/Speex
        || b.starts_with(b"MAC ")                               // Monkey's Audio (APE)
        || b.starts_with(b"wvpk")                               // WavPack
        || b.starts_with(b"MPCK")                               // Musepack SV8
        || b.starts_with(b"MP+")                                // Musepack SV7
        || b.starts_with(b"DSD ")                               // DSD stream (.dsf — ID3v2 cover)
        // MP4/M4A audio: ftyp with an AUDIO brand. Crucially this excludes the
        // image MP4 brands (heic/heix/mif1/avif/…) so HEIC/AVIF still take the
        // normal image path instead of being misrouted here.
        || (b.len() >= 12
            && &b[4..8] == b"ftyp"
            && matches!(&b[8..12], b"M4A " | b"M4B " | b"M4P " | b"mp42" | b"mp41" | b"isom" | b"iso2" | b"dash"))
        || (b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WAVE") // WAV
        || (b.len() >= 12 && &b[0..4] == b"FORM" && matches!(&b[8..12], b"AIFF" | b"AIFC")) // AIFF
        || b.starts_with(&[0x30, 0x26, 0xB2, 0x75])             // ASF / WMA
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smallest bytes that pass `looks_like_raster` as a JPEG (so the extracted art
    /// is "decodable" without shipping a real image fixture).
    const FAKE_JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F'];

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    /// A `WM/Picture` byte-array value (front cover) wrapping `image`.
    fn wm_picture(image: &[u8]) -> Vec<u8> {
        let mut v = vec![3u8]; // picture type 3 = front cover
        v.extend_from_slice(&(image.len() as u32).to_le_bytes());
        v.extend_from_slice(&utf16("image/jpeg")); // MIME
        v.extend_from_slice(&[0, 0]); // NUL
        v.extend_from_slice(&[0, 0]); // empty description + NUL
        v.extend_from_slice(image);
        v
    }

    fn asf_object(guid: [u8; 16], payload: &[u8]) -> Vec<u8> {
        let mut o = guid.to_vec();
        o.extend_from_slice(&((24 + payload.len()) as u64).to_le_bytes());
        o.extend_from_slice(payload);
        o
    }

    /// Wrap nested objects in a Header Extension Object (where real files put the
    /// Metadata / Metadata Library Objects): reserved GUID(16) + reserved u16(6) +
    /// data-size u32 + nested.
    fn hdr_ext(nested: &[u8]) -> Vec<u8> {
        let mut payload = vec![0u8; 16]; // reserved GUID (contents irrelevant — we skip it)
        payload.extend_from_slice(&6u16.to_le_bytes());
        payload.extend_from_slice(&(nested.len() as u32).to_le_bytes());
        payload.extend_from_slice(nested);
        asf_object(ASF_HDR_EXT_GUID, &payload)
    }

    /// Wrap one header sub-object in a complete ASF Header Object.
    fn asf_file(sub: &[u8]) -> Vec<u8> {
        let mut h = ASF_HEADER_GUID.to_vec();
        h.extend_from_slice(&((30 + sub.len()) as u64).to_le_bytes()); // header object size
        h.extend_from_slice(&1u32.to_le_bytes()); // number of header objects
        h.extend_from_slice(&[1, 2]); // reserved
        h.extend_from_slice(sub);
        h
    }

    /// Picture in the Extended Content Description Object (the small-cover path,
    /// `value-len` is u16). This is where mutagen/encoders put covers ≤ 64 KiB.
    fn ecd_payload(name: &str, value: &[u8]) -> Vec<u8> {
        let nm = {
            let mut n = utf16(name);
            n.extend_from_slice(&[0, 0]);
            n
        };
        let mut p = 1u16.to_le_bytes().to_vec(); // descriptor count
        p.extend_from_slice(&(nm.len() as u16).to_le_bytes());
        p.extend_from_slice(&nm);
        p.extend_from_slice(&1u16.to_le_bytes()); // value type 1 = byte array
        p.extend_from_slice(&(value.len() as u16).to_le_bytes());
        p.extend_from_slice(value);
        p
    }

    /// Picture in the Metadata Library Object (the full-size path, `data-len` is u32).
    fn mdlib_payload(name: &str, value: &[u8]) -> Vec<u8> {
        let nm = {
            let mut n = utf16(name);
            n.extend_from_slice(&[0, 0]);
            n
        };
        let mut p = 1u16.to_le_bytes().to_vec(); // record count
        p.extend_from_slice(&0u16.to_le_bytes()); // language list index
        p.extend_from_slice(&0u16.to_le_bytes()); // stream number
        p.extend_from_slice(&(nm.len() as u16).to_le_bytes());
        p.extend_from_slice(&1u16.to_le_bytes()); // data type 1 = byte array
        p.extend_from_slice(&(value.len() as u32).to_le_bytes());
        p.extend_from_slice(&nm);
        p.extend_from_slice(value);
        p
    }

    /// Build a 4-byte ID3v2 synchsafe size (high bit of each byte is zero).
    fn synchsafe_bytes(n: u32) -> [u8; 4] {
        [((n >> 21) & 0x7f) as u8, ((n >> 14) & 0x7f) as u8, ((n >> 7) & 0x7f) as u8, (n & 0x7f) as u8]
    }

    /// A minimal `.dsf`: the 28-byte `DSD ` header pointing at a trailing ID3v2.4 tag
    /// whose single `APIC` frame (front cover) wraps `image`.
    fn dsf_with_cover(image: &[u8]) -> Vec<u8> {
        let mut apic = vec![0u8]; // text encoding: latin1
        apic.extend_from_slice(b"image/jpeg\0"); // MIME
        apic.push(3); // picture type: front cover
        apic.push(0); // empty description (latin1 NUL)
        apic.extend_from_slice(image);
        let mut frame = b"APIC".to_vec();
        frame.extend_from_slice(&synchsafe_bytes(apic.len() as u32));
        frame.extend_from_slice(&[0, 0]); // frame flags
        frame.extend_from_slice(&apic);
        let mut id3 = b"ID3".to_vec();
        id3.extend_from_slice(&[4, 0, 0]); // v2.4.0, no flags
        id3.extend_from_slice(&synchsafe_bytes(frame.len() as u32));
        id3.extend_from_slice(&frame);
        let mut f = b"DSD ".to_vec();
        f.extend_from_slice(&28u64.to_le_bytes()); // DSD chunk size
        f.extend_from_slice(&(28 + id3.len() as u64).to_le_bytes()); // total file size
        f.extend_from_slice(&28u64.to_le_bytes()); // metadata pointer → right after the header
        f.extend_from_slice(&id3);
        f
    }

    /// DSD `.dsf` carries its cover in a trailing ID3v2 tag that lofty can't read; the
    /// hand-rolled `dsf_cover` must pull the front-cover APIC out.
    #[test]
    fn dsf_cover_reads_id3v2_apic() {
        assert_eq!(extract(&dsf_with_cover(FAKE_JPEG)), Some(FAKE_JPEG.to_vec()));
    }

    #[test]
    fn asf_cover_reads_wm_picture_from_ecd() {
        let pic = wm_picture(FAKE_JPEG);
        let file = asf_file(&asf_object(ASF_ECD_GUID, &ecd_payload("WM/Picture", &pic)));
        assert_eq!(asf_cover(&mut Cursor::new(file)), Some(FAKE_JPEG.to_vec()));
    }

    #[test]
    fn asf_cover_reads_wm_picture_from_metadata_library() {
        // Real files nest the Metadata Library Object inside the Header Extension
        // Object — the case that the first implementation missed.
        let pic = wm_picture(FAKE_JPEG);
        let mdlib = asf_object(ASF_MDLIB_GUID, &mdlib_payload("WM/Picture", &pic));
        let file = asf_file(&hdr_ext(&mdlib));
        assert_eq!(asf_cover(&mut Cursor::new(file)), Some(FAKE_JPEG.to_vec()));
    }

    #[test]
    fn asf_cover_reads_wm_picture_from_metadata_object() {
        // The older Metadata Object (same record layout) also nests in Header Extension.
        let pic = wm_picture(FAKE_JPEG);
        let meta = asf_object(ASF_META_GUID, &mdlib_payload("WM/Picture", &pic));
        let file = asf_file(&hdr_ext(&meta));
        assert_eq!(asf_cover(&mut Cursor::new(file)), Some(FAKE_JPEG.to_vec()));
    }

    #[test]
    fn asf_cover_ignores_non_picture_attributes_and_non_asf() {
        // A WM/Picture whose payload isn't a decodable raster → rejected.
        let junk = wm_picture(&[0u8; 16]);
        let file = asf_file(&asf_object(ASF_MDLIB_GUID, &mdlib_payload("WM/Picture", &junk)));
        assert_eq!(asf_cover(&mut Cursor::new(file)), None);
        // A non-picture attribute name → ignored.
        let pic = wm_picture(FAKE_JPEG);
        let file = asf_file(&asf_object(ASF_ECD_GUID, &ecd_payload("WM/Author", &pic)));
        assert_eq!(asf_cover(&mut Cursor::new(file)), None);
        // Non-ASF bytes bail immediately.
        assert_eq!(asf_cover(&mut Cursor::new(b"ID3\x04not an asf file".to_vec())), None);
    }

    #[test]
    fn name_matcher_is_exact() {
        let mut wp = utf16("WM/Picture");
        assert!(name_eq(&wp, b"WM/Picture"));
        wp.extend_from_slice(&[0, 0]); // trailing NUL is allowed
        assert!(name_eq(&wp, b"WM/Picture"));
        assert!(!name_eq(&utf16("WM/PictureX"), b"WM/Picture"));
        assert!(!name_eq(&utf16("WM/Author"), b"WM/Picture"));
        assert!(!name_eq(b"WM/Picture", b"WM/Picture")); // ASCII (not UTF-16) → no
    }

    /// Multiple header sub-objects (the tag tests need Content Description + ECD).
    fn asf_file_n(subs: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = subs.iter().flatten().copied().collect();
        let mut h = ASF_HEADER_GUID.to_vec();
        h.extend_from_slice(&((30 + body.len()) as u64).to_le_bytes());
        h.extend_from_slice(&(subs.len() as u32).to_le_bytes());
        h.extend_from_slice(&[1, 2]);
        h.extend_from_slice(&body);
        h
    }

    fn utf16z(s: &str) -> Vec<u8> {
        let mut v = utf16(s);
        v.extend_from_slice(&[0, 0]);
        v
    }

    /// Content Description Object payload (Title + Author; other fields empty).
    fn cd_payload(title: &str, author: &str) -> Vec<u8> {
        let (t, a) = (utf16z(title), utf16z(author));
        let mut p = (t.len() as u16).to_le_bytes().to_vec();
        p.extend_from_slice(&(a.len() as u16).to_le_bytes());
        p.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // copyright/description/rating lengths = 0
        p.extend_from_slice(&t);
        p.extend_from_slice(&a);
        p
    }

    /// Extended Content Description payload of Unicode-string attributes.
    fn ecd_str_payload(pairs: &[(&str, &str)]) -> Vec<u8> {
        let mut p = (pairs.len() as u16).to_le_bytes().to_vec();
        for (name, val) in pairs {
            let (nm, vv) = (utf16z(name), utf16z(val));
            p.extend_from_slice(&(nm.len() as u16).to_le_bytes());
            p.extend_from_slice(&nm);
            p.extend_from_slice(&0u16.to_le_bytes()); // value type 0 = Unicode string
            p.extend_from_slice(&(vv.len() as u16).to_le_bytes());
            p.extend_from_slice(&vv);
        }
        p
    }

    #[test]
    fn asf_tags_reads_content_description_and_wm_attrs() {
        let cd = asf_object(ASF_CONTENT_DESC_GUID, &cd_payload("My Song", "The Artist"));
        let ecd = asf_object(
            ASF_ECD_GUID,
            &ecd_str_payload(&[("WM/AlbumTitle", "The Album"), ("WM/TrackNumber", "7/12")]),
        );
        let tags = asf_tags(&mut Cursor::new(asf_file_n(&[cd, ecd]))).unwrap();
        assert_eq!(tags.title.as_deref(), Some("My Song"));
        assert_eq!(tags.artist.as_deref(), Some("The Artist"));
        assert_eq!(tags.album.as_deref(), Some("The Album"));
        assert_eq!(tags.track, Some(7));
    }

    #[test]
    fn asf_tags_reads_attrs_nested_in_metadata_library() {
        // Album/track can live in the Header-Extension-nested Metadata Library too.
        let mdlib = asf_object(ASF_MDLIB_GUID, &mdlib_str_payload("WM/AlbumTitle", "Nested Album"));
        let tags = asf_tags(&mut Cursor::new(asf_file_n(&[hdr_ext(&mdlib)]))).unwrap();
        assert_eq!(tags.album.as_deref(), Some("Nested Album"));
    }

    /// One Metadata Library record holding a Unicode-string attribute.
    fn mdlib_str_payload(name: &str, val: &str) -> Vec<u8> {
        let (nm, vv) = (utf16z(name), utf16z(val));
        let mut p = 1u16.to_le_bytes().to_vec(); // record count
        p.extend_from_slice(&0u16.to_le_bytes()); // language list index
        p.extend_from_slice(&0u16.to_le_bytes()); // stream number
        p.extend_from_slice(&(nm.len() as u16).to_le_bytes());
        p.extend_from_slice(&0u16.to_le_bytes()); // data type 0 = Unicode string
        p.extend_from_slice(&(vv.len() as u32).to_le_bytes());
        p.extend_from_slice(&nm);
        p.extend_from_slice(&vv);
        p
    }

    #[test]
    fn asf_tags_prefers_track_author_over_album_artist() {
        // Real files store the ECD before the Content Description Object, so without
        // care WM/AlbumArtist would win. The track Author must win regardless of order.
        let ecd = asf_object(ASF_ECD_GUID, &ecd_str_payload(&[("WM/AlbumArtist", "Various Artists")]));
        let cd = asf_object(ASF_CONTENT_DESC_GUID, &cd_payload("Song", "Real Artist"));
        let tags = asf_tags(&mut Cursor::new(asf_file_n(&[ecd, cd]))).unwrap();
        assert_eq!(tags.artist.as_deref(), Some("Real Artist"));
    }

    #[test]
    fn asf_tags_none_for_non_asf() {
        assert!(asf_tags(&mut Cursor::new(b"ID3\x04 not an asf file".to_vec())).is_none());
    }
}
