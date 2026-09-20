//! Smart targeted read for MP4/MOV video thumbnails — build a tiny self-contained MP4 that
//! holds exactly ONE video keyframe (the sync sample nearest ~30 % of the running time) by
//! parsing the source file's `moov` sample tables, then hand that mini-MP4 to Media
//! Foundation ([`crate::video::frame_from_bytes`]).
//!
//! Why this exists: the bounded-prefix / remux tiers in `thumbprovider.rs` can only reach an
//! *early* frame (the data past the bounded head simply isn't in the buffer), so the thumbnail
//! is usually the studio intro / a fade-in — useless for identifying the video. The targeted
//! read uses the file's own index to seek straight to a representative mid-video keyframe.
//!
//! Why it's also *faster*, not slower: instead of pulling a 64–128 MB head off the disk to
//! reach an early frame, we read only the `moov` index (typically ~1–2 MB) plus that one
//! keyframe (~1–5 MB) — single-digit MB, one seek + one small read. Using the index means VBR
//! doesn't matter (no time→byte estimation), and it retires the need for the 128 MB remux head
//! on moov-at-end files. The original 30 s meltdown was Media Foundation doing *thousands* of
//! tiny random reads through the shell's marshaled `IStream`; this is the opposite shape.
//!
//! Everything is best-effort: a fragmented MP4 (samples live in `moof`, not `moov`), a
//! `stz2`/`co64` layout we can't map, a non-ISO-BMFF container, or any short read returns
//! `None` and the caller falls back to the bounded-prefix path — never worse than before.
//!
//! ISO/IEC 14496-12 (ISO base media file format) box references throughout: `moov` ▸ `trak`
//! (the `vide` handler) ▸ `mdia` ▸ `minf` ▸ `stbl` ▸ { `stsd`, `stts`, `stss`, `stsc`,
//! `stsz`/`stz2`, `stco`/`co64` }.

use std::io::{Read, Seek, SeekFrom};
mod sampletable;
use sampletable::*;

/// Sanity cap on the `moov` index we'll pull into memory. A movie index is normally a few MB;
/// anything past this is malformed or hostile, so we bail to the fallback tier.
const MOOV_MAX: u64 = 96 * 1024 * 1024;
/// Sanity cap on a single keyframe sample. Even an 8K intra frame is well under this; a larger
/// claimed size means a corrupt sample table, so we bail rather than allocate it.
const KEYFRAME_MAX: u64 = 64 * 1024 * 1024;
/// Largest plausible `ftyp` box (it's normally 16–40 bytes); past this we synthesize our own.
const FTYP_MAX: u64 = 1024;
/// Cap on top-level boxes `scan_top_level` will examine, mirroring flv.rs's `MAX_TAGS` /
/// mkv.rs's `0..64` walk cap. Without it the loop is bounded only by total file size: a
/// file built from many tiny top-level boxes (or with no `moov` at all) drives roughly
/// `total/8` iterations, each a Seek+Read pair straight onto the marshaled COM `IStream`
/// with no buffering layer, on the calling apartment thread. A real file has a handful of
/// top-level boxes (`ftyp`/`moov`/`mdat`/`free`/…), so this never bites legitimate input.
const MAX_TOP_LEVEL_BOXES: u32 = 4096;

/// Build a one-keyframe mini-MP4 for the sync sample nearest `fraction` of the running time.
/// `r` is the source video (the shell `IStream`, a file, or in tests a `Cursor`). Returns the
/// mini-MP4 bytes for [`crate::video::frame_from_bytes`] plus the display rotation this same
/// `moov`/`tkhd` already carried — a caller that already has this need not
/// re-read the moov a second time through [`display_rotation`] just to ask), or `None` if the
/// source isn't a parseable ISO-BMFF with an indexed video track (caller falls back to the
/// prefix path).
pub fn keyframe_mini_mp4<R: Read + Seek>(
    r: &mut R,
    fraction: f64,
) -> Option<(Vec<u8>, Option<u32>)> {
    let (total, ftyp, moov) = scan_top_level(r)?;

    // --- Locate the video track's sample tables inside the moov ------------------------------
    let trak = video_trak(box_body(&moov))?;
    let mdia_body = box_body(find(trak, b"mdia")?);
    let minf = find(mdia_body, b"minf")?;
    let stbl = box_body(find(box_body(minf), b"stbl")?);

    let stsd = find(stbl, b"stsd")?; // copied verbatim — carries avcC/hvcC codec config
    let stts = find(stbl, b"stts")?;
    let stsc = find(stbl, b"stsc")?;
    let stss = find(stbl, b"stss"); // optional: absent ⇒ every sample is a sync sample
    let chunks = find(stbl, b"stco")
        .map(|b| (b, false))
        .or_else(|| find(stbl, b"co64").map(|b| (b, true)))?;
    let sizes = find(stbl, b"stsz")
        .map(SampleSizes::Stsz)
        .or_else(|| find(stbl, b"stz2").map(SampleSizes::Stz2))?;

    let media_timescale = find(mdia_body, b"mdhd")
        .and_then(mdhd_timescale)
        .unwrap_or(1000);

    // --- Map 30 %-of-duration → decoding-order sample → nearest preceding sync sample --------
    let (target_sample, frame_delta) = stts_target(full_box_body(stts), fraction)?;
    let kf_sample0 = nearest_sync(stss, target_sample + 1)?.saturating_sub(1); // back to 0-based

    let kf_size = sizes.size_of(kf_sample0)?;
    if kf_size == 0 || kf_size > KEYFRAME_MAX {
        return None;
    }
    let (kf_offset, desc_index) = sample_location(full_box_body(stsc), chunks, &sizes, kf_sample0)?;

    // --- Read just that keyframe's bytes -----------------------------------------------------
    if kf_offset.checked_add(kf_size)? > total {
        return None;
    }
    let mut keyframe = vec![0u8; kf_size as usize];
    read_exact_at(r, kf_offset, &mut keyframe)?;

    // --- Coded dimensions from the visual sample entry (display hints for tkhd/mvhd) ---------
    let (width, height) = visual_dims(stsd).unwrap_or((1920, 1080));

    // The rotation is a pure function of the SAME `tkhd` this walk already located to find
    // `mdia` — read it here instead of making every caller re-scan the moov a second time
    // (`display_rotation`) just to ask.
    let rotation = find(trak, b"tkhd").and_then(rotation_from_tkhd);

    Some((
        build_mini_mp4(
            ftyp.as_deref(),
            stsd,
            desc_index,
            frame_delta.max(1),
            media_timescale,
            width,
            height,
            &keyframe,
        ),
        rotation,
    ))
}

/// One top-level box header at `pos`: its fourcc and its full (header+body) size, honoring
/// the extended 8-byte size field when `size32 == 1`. `None` when the header can't be read or
/// its size can't be resolved — the caller treats that as "stop walking", not "reject the
/// file", since whatever was already found (a `moov` from an earlier box) still stands.
fn read_top_level_header<R: Read + Seek>(
    r: &mut R,
    pos: u64,
    total: u64,
) -> Option<([u8; 4], u64)> {
    let mut hdr = [0u8; 8];
    read_exact_at(r, pos, &mut hdr)?;
    let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let typ = [hdr[4], hdr[5], hdr[6], hdr[7]];
    // Only read the extended 8-byte size field when the header actually needs it — the
    // shared `decode_box_size` treats a missing `extended` as "decline size32 == 1".
    let extended = if size32 == 1 {
        let mut ext = [0u8; 8];
        read_exact_at(r, pos + 8, &mut ext)?;
        Some(u64::from_be_bytes(ext))
    } else {
        None
    };
    let (full, _header_len) =
        crate::container::boxhdr::decode_box_size(size32, extended, pos, total)?;
    Some((typ, full))
}

/// Walk the top-level boxes: the `ftyp` gate (rejecting non-ISO-BMFF cheaply), the verbatim
/// `ftyp` copy when it is sanely sized, and the whole `moov` read into RAM (capped).
/// Returns `(total_size, ftyp, moov)`.
fn scan_top_level<R: Read + Seek>(r: &mut R) -> Option<(u64, Option<Vec<u8>>, Vec<u8>)> {
    let total = r.seek(SeekFrom::End(0)).ok()?;
    if total < 16 {
        return None;
    }
    let mut pos: u64 = 0;
    let mut ftyp: Option<Vec<u8>> = None;
    let mut moov_range: Option<(u64, u64)> = None;
    let mut boxes_seen: u32 = 0;
    while pos + 8 <= total {
        boxes_seen += 1;
        if boxes_seen > MAX_TOP_LEVEL_BOXES {
            return None;
        }
        let Some((typ, full)) = read_top_level_header(r, pos, total) else {
            break;
        };
        // ISO-BMFF gate: a real mp4/mov starts with `ftyp`. This cheaply rejects Matroska/AVI/
        // ASF/FLV/MPEG (their leading bytes are not a sane `ftyp` box), so the caller's broader
        // `is_video_magic` sniff still routes those to the bounded-prefix path.
        if pos == 0 && &typ != b"ftyp" {
            return None;
        }
        match &typ {
            b"ftyp" if full <= FTYP_MAX => {
                let mut fb = vec![0u8; full as usize];
                read_exact_at(r, pos, &mut fb)?;
                ftyp = Some(fb);
            }
            b"moov" => {
                moov_range = Some((pos, full));
                break; // moov found (faststart: right after ftyp; else: after mdat) — stop walking
            }
            _ => {}
        }
        pos = pos.checked_add(full)?;
    }

    let (moov_off, moov_size) = moov_range?;
    if moov_size == 0 || moov_size > MOOV_MAX {
        return None;
    }
    let mut moov = vec![0u8; moov_size as usize];
    read_exact_at(r, moov_off, &mut moov)?;
    Some((total, ftyp, moov))
}

/// The `mdia` body of the video track (the trak whose `hdlr` handler_type is 'vide').
fn video_mdia(moov_body: &[u8]) -> Option<&[u8]> {
    find(video_trak(moov_body)?, b"mdia").map(box_body)
}

/// The `trak` BODY of the file's video track, which is what a caller needs when it wants
/// something outside `mdia` — the `tkhd` display matrix, in [`display_rotation`]'s case.
fn video_trak(moov_body: &[u8]) -> Option<&[u8]> {
    for (typ, trak) in boxes(moov_body) {
        if &typ != b"trak" {
            continue;
        }
        let trak_body = box_body(trak);
        // `continue`, not `?`: a trak with no `mdia` is one BAD track, not the end of the
        // search. Propagating None here abandoned the whole moov on the first oddball trak
        // (editors emit hint/metadata/placeholder traks), so a file whose video track came
        // second reported "no video track" — the smart index tier silently downgraded, and
        // doctor told the user the codec was unidentifiable in a file that plainly has one.
        let Some(mdia) = find(trak_body, b"mdia") else {
            continue;
        };
        let hdlr = match find(box_body(mdia), b"hdlr") {
            Some(h) => full_box_body(h),
            None => continue,
        };
        // hdlr: pre_defined(4) handler_type(4) … — 'vide' marks the video track.
        if hdlr.get(4..8) == Some(b"vide") {
            return Some(trak_body);
        }
    }
    None
}

/// The CLOCKWISE rotation, in degrees (90, 180 or 270), that this video's `tkhd` display
/// matrix asks a player to apply — or `None` for an upright video and for anything this
/// cannot read as one of the three right angles.
///
/// Issue #32. Rotating a phone or action-cam clip losslessly means writing this matrix and
/// touching not one pixel (`ffmpeg -display_rotation 90 -i in.mp4 -c copy out.mp4`), which is
/// instant and keeps the original quality. Windows' own thumbnailer honours it and so does
/// essentially every player, so a thumbnail that ignores it is not merely different, it is
/// the one place the user's library disagrees with itself.
///
/// **Only exact right angles.** The matrix is a general affine transform and can express
/// scales, shears and arbitrary angles; a thumbnail cannot honour those faithfully, and
/// half-honouring one would be worse than leaving it alone. So this recognises the four
/// canonical forms and declines everything else, including the identity.
///
/// The translation components are deliberately ignored: a 90/270 rotation about the origin
/// normally comes with a translate that puts the picture back in frame, but we rotate the
/// decoded bitmap itself rather than composing pixels through the matrix, so the offset has
/// nothing to act on.
pub fn display_rotation<R: Read + Seek>(r: &mut R) -> Option<u32> {
    let (_, _, moov) = scan_top_level(r)?;
    let trak = video_trak(box_body(&moov))?;
    rotation_from_tkhd(find(trak, b"tkhd")?)
}

/// [`display_rotation`]'s pure half: the clockwise angle encoded in one whole `tkhd` box.
///
/// `tkhd` is a full box, and the matrix sits after fields whose width depends on its version:
/// v0 packs creation/modification/duration as 32-bit (20 bytes through `duration`), v1 as
/// 64-bit (32 bytes), and both then carry 16 bytes of reserved/layer/alternate_group/volume
/// before the nine 32-bit matrix entries.
fn rotation_from_tkhd(tkhd: &[u8]) -> Option<u32> {
    let p = box_body(tkhd); // [version][flags(3)] then the version-dependent fields
    let matrix_at = match p.first()? {
        0 => 4 + 20 + 16,
        1 => 4 + 32 + 16,
        _ => return None,
    };
    // a, b, c, d — the 2x2 that carries rotation. u/v/w and the translation are skipped:
    // see the doc comment above for why.
    let a = g32(p, matrix_at)? as i32;
    let b = g32(p, matrix_at + 4)? as i32;
    let c = g32(p, matrix_at + 12)? as i32;
    let d = g32(p, matrix_at + 16)? as i32;
    rotation_from_matrix(a, b, c, d)
}

/// One unit in the matrix's 16.16 fixed-point encoding.
const FIXED_ONE: i32 = 1 << 16;

/// [`rotation_from_matrix`], reachable from `mkv`'s tests so the two containers' rotation
/// mappings are asserted against EACH OTHER rather than each restating its own answer. They
/// encode the same intent with opposite signs, so a drift in one of them is exactly the bug
/// that would rotate one container the wrong way while the other stayed right.
#[cfg(test)]
pub(crate) fn rotation_from_matrix_for_tests(a: i32, b: i32, c: i32, d: i32) -> Option<u32> {
    rotation_from_matrix(a, b, c, d)
}

/// The clockwise angle for the 2x2 part of a display matrix, or `None` when it is the
/// identity or anything that is not an exact right-angle rotation.
///
/// **The mapping is measured, not derived from a convention.** FFmpeg, the ISO spec and
/// various players describe this transform with opposite sign conventions, and getting it
/// backwards would rotate every phone video the wrong way — a bug that looks exactly like the
/// one being fixed. So the four rows below were read out of files written by the very command
/// the issue reports, and each one's intended picture was taken from `ffmpeg -frames:v 1`
/// (whose autorotate is on by default), then matched against rotations of the unrotated
/// original. All three matched at a mean pixel difference of **0.00**:
///
/// ```text
///   a      b      c      d     written by                 intended picture
///   1      0      0      1     (no rotation)              unchanged
///   0     -1      1      0     -display_rotation 90       90 deg COUNTER-clockwise
///  -1      0      0     -1     -display_rotation 180      180
///   0      1     -1      0     -display_rotation 270      90 deg clockwise
/// ```
///
/// `rotation_matches_the_measured_ground_truth` pins those exact rows.
fn rotation_from_matrix(a: i32, b: i32, c: i32, d: i32) -> Option<u32> {
    match (a, b, c, d) {
        (0, x, y, 0) if x == -FIXED_ONE && y == FIXED_ONE => Some(270),
        (x, 0, 0, y) if x == -FIXED_ONE && y == -FIXED_ONE => Some(180),
        (0, x, y, 0) if x == FIXED_ONE && y == -FIXED_ONE => Some(90),
        _ => None,
    }
}

/// Sanity cap on an embedded cover image. Poster art is tens to hundreds of KB; a larger
/// `covr` payload means a corrupt or hostile atom, so bail instead of allocating it.
const COVER_MAX: usize = 32 * 1024 * 1024;

/// The iTunes-style cover artwork of an MP4/MOV, i.e. the `covr` item every media manager
/// (iTunes, Plex, Jellyfin, MusicBrainz taggers) writes. The Matroska twin is
/// [`crate::mkv::attached_cover`]; [`crate::vcodec::cover_art`] tries both.
///
/// Path: `moov` ▸ `udta` ▸ `meta` ▸ `ilst` ▸ `covr` ▸ `data`. The `data` payload is preceded
/// by an 8-byte header (a 4-byte type indicator, 13 = JPEG and 14 = PNG, then a 4-byte
/// locale), which is stripped here so the caller receives plain image bytes.
pub fn cover_art<R: Read + Seek>(r: &mut R) -> Option<Vec<u8>> {
    let (_, _, moov) = scan_top_level(r)?;
    let udta = find(box_body(&moov), b"udta")?;
    let meta = find(box_body(udta), b"meta")?;
    // `meta` is a FULL box in the iTunes/ISO layout (4 bytes of version+flags before its
    // children) but a plain box in some QuickTime writers' output. Try the full-box reading
    // first and fall back, rather than assuming and silently finding nothing.
    let ilst = find(full_box_body(meta), b"ilst").or_else(|| find(box_body(meta), b"ilst"))?;
    let covr = find(box_body(ilst), b"covr")?;
    let data = find(box_body(covr), b"data")?;
    // data: version+flags(4) locale(4) then the image bytes.
    let payload = box_body(data).get(8..)?;
    if payload.is_empty() || payload.len() > COVER_MAX || !looks_like_cover_image(payload) {
        return None;
    }
    Some(payload.to_vec())
}

/// Does this payload actually start like an image we can decode? The `data` type indicator
/// claims JPEG or PNG, but it is metadata a writer can get wrong, so trust the bytes: a
/// mislabelled or empty blob should fall through to the normal tiers rather than be handed
/// on as "the cover" and fail there.
fn looks_like_cover_image(b: &[u8]) -> bool {
    b.starts_with(&[0xFF, 0xD8, 0xFF])                          // JPEG
        || b.starts_with(&[0x89, b'P', b'N', b'G'])             // PNG
        || b.starts_with(b"GIF8")                               // GIF
        || b.starts_with(b"BM")                                 // BMP
        || (b.len() >= 12 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP")
}

/// Walk `moov` ▸ `mdia` ▸ `minf` ▸ `stbl` ▸ `stsd` down to the video track's sample
/// description table and pass it to `f`. Reads the `ftyp` gate + `moov` only; `None` for a
/// non-ISO-BMFF source or a video-less file.
fn with_video_stsd<R: Read + Seek, T>(r: &mut R, f: impl FnOnce(&[u8]) -> Option<T>) -> Option<T> {
    let (_, _, moov) = scan_top_level(r)?;
    let mdia_body = video_mdia(box_body(&moov))?;
    let minf = find(mdia_body, b"minf")?;
    let stbl = box_body(find(box_body(minf), b"stbl")?);
    let stsd = find(stbl, b"stsd")?;
    f(stsd)
}

/// The sample-entry fourcc of the video track's first `stsd` entry (`avc1`, `hvc1`, `av01`,
/// …), for the doctor's codec diagnosis. Reads the `ftyp` gate + `moov` only; `None` for
/// non-ISO-BMFF sources or video-less files.
pub fn video_codec_fourcc<R: Read + Seek>(r: &mut R) -> Option<[u8; 4]> {
    with_video_stsd(r, |stsd| {
        // stsd: header(8) version+flags(4) entry_count(4) | entry: size(4) type(4) …
        let t = stsd.get(20..24)?;
        Some([t[0], t[1], t[2], t[3]])
    })
}

/// The fixed part of a `VisualSampleEntry`: the 8-byte box header plus 78 bytes of fields
/// (reserved, data-reference index, dimensions, resolution, frame count, compressor name,
/// depth). The codec configuration boxes (`avcC`, `hvcC`, `pasp`, ...) follow as children.
const VISUAL_SAMPLE_ENTRY_LEN: usize = 8 + 78;

/// The `profile_idc` of the video track's H.264 decoder configuration (`stsd` ▸ `avc1` /
/// `avc3` ▸ `avcC`, the `AVCProfileIndication` byte), or `None` for a non-ISO-BMFF source, a
/// track that is not H.264, or an entry with no readable `avcC`. Reads the `ftyp` gate +
/// `moov` only. Feeds [`crate::vcodec::mf_undecodable_reason`] (issue #35), which is why it
/// is deliberately a byte read and not a decode: the whole point is answering "can Windows
/// decode this" without asking Windows to try.
pub fn h264_profile_idc<R: Read + Seek>(r: &mut R) -> Option<u8> {
    with_video_stsd(r, |stsd| {
        // stsd: header(8) version+flags(4) entry_count(4) | entries, each a box of its own.
        let (typ, entry) = boxes(stsd.get(16..)?).next()?;
        if &typ != b"avc1" && &typ != b"avc3" {
            return None;
        }
        let children = entry.get(VISUAL_SAMPLE_ENTRY_LEN..)?;
        let avcc = find(children, b"avcC")?;
        // AVCDecoderConfigurationRecord: configurationVersion, AVCProfileIndication, ...
        box_body(avcc).get(1).copied()
    })
}

// ---------------------------------------------------------------------------------------------
// Box navigation over an in-RAM moov slice
// ---------------------------------------------------------------------------------------------

/// Iterate the immediate child boxes of `buf`, yielding `(type, full_box_bytes)`. Stops at the
/// first malformed/overrunning length so a truncated or hostile index can't loop or over-read.
fn boxes(buf: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        if pos + 8 > buf.len() {
            return None;
        }
        let size32 = u32::from_be_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]);
        let typ = [buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]];
        let extended = (size32 == 1).then(|| g64(buf, pos + 8)).flatten();
        // `decode_box_size` does the checked arithmetic (`pos + full` via `checked_add`) that
        // matters here: the 64-bit extended-size form (`size32 == 1`) puts a file-controlled u64
        // straight into `full`, and the release profile builds with overflow-checks off — so an
        // unchecked `full = u64::MAX` would wrap `pos + full` to `pos - 1`, which SATISFIES a
        // naive bounds test and then panics in the slice on the next line. Under `panic = "abort"`
        // in the in-process context-menu path that aborts explorer.exe itself.
        let (full, _header_len) = crate::container::boxhdr::decode_box_size(
            size32,
            extended,
            pos as u64,
            buf.len() as u64,
        )?;
        let end = pos.checked_add(full as usize)?;
        let out = &buf[pos..end];
        pos = end;
        Some((typ, out))
    })
}

/// First child box of `buf` with type `typ` (full box bytes incl. header).
fn find<'a>(buf: &'a [u8], typ: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(buf).find(|(t, _)| t == typ).map(|(_, f)| f)
}

/// The payload of a box (skips the 8- or 16-byte size/type header).
///
/// A slice too short to hold even the size field yields an empty body rather than panicking.
/// Every caller that comes from [`boxes`] is handed a box of at least 8 bytes, so this cannot
/// fire there — but the indexing it replaces was an unchecked `full[0..4]`, and this crate
/// runs inside `explorer.exe` under `panic = "abort"`, where an out-of-bounds read on
/// attacker-controlled bytes aborts the user's shell rather than declining a thumbnail. A
/// parser reached with a short slice must decline, and the caller must not have to know.
fn box_body(full: &[u8]) -> &[u8] {
    let Some(size_field) = full.get(0..4) else {
        return &[];
    };
    let size32 = u32::from_be_bytes(size_field.try_into().unwrap_or([0; 4]));
    let hlen = if size32 == 1 { 16 } else { 8 };
    full.get(hlen..).unwrap_or(&[])
}

/// The payload of a *full* box (skips the header + the 1-byte version + 3-byte flags).
fn full_box_body(full: &[u8]) -> &[u8] {
    box_body(full).get(4..).unwrap_or(&[])
}

fn g16(s: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_be_bytes(s.get(off..off + 2)?.try_into().ok()?))
}
fn g32(s: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_be_bytes(s.get(off..off + 4)?.try_into().ok()?))
}
fn g64(s: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_be_bytes(s.get(off..off + 8)?.try_into().ok()?))
}

// ---------------------------------------------------------------------------------------------
// Sample-table interpretation
// ---------------------------------------------------------------------------------------------

/// Media timescale from `mdhd` (ticks per second). `full` is the whole mdhd box.
fn mdhd_timescale(full: &[u8]) -> Option<u32> {
    let p = box_body(full); // [version][flags(3)] then the v0/v1 fields
    match p.first()? {
        1 => g32(p, 20), // v1: creation(8) modification(8) timescale(4)
        _ => g32(p, 12), // v0: creation(4) modification(4) timescale(4)
    }
}

/// Coded width/height from the first visual sample entry in `stsd` (the whole stsd box).
/// Layout: header(8) version+flags(4) entry_count(4) | entry: size(4) type(4) reserved(6)
/// data_ref(2) pre_defined(2) reserved(2) pre_defined(12) width(2) height(2) …
fn visual_dims(stsd: &[u8]) -> Option<(u16, u16)> {
    let w = g16(stsd, 48)?;
    let h = g16(stsd, 50)?;
    ((1..=16384).contains(&w) && (1..=16384).contains(&h)).then_some((w, h))
}

// ---------------------------------------------------------------------------------------------
// Mini-MP4 muxing
// ---------------------------------------------------------------------------------------------

/// `[size][type][payload]` box. `pub(crate)` for `flv`'s hand-built `stsd`.
pub(crate) fn bx(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + payload.len());
    v.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    v.extend_from_slice(typ);
    v.extend_from_slice(payload);
    v
}

/// `[size][type][version][flags(3)][body]` full box. `pub(crate)` for `flv`.
pub(crate) fn fbx(typ: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(4 + body.len());
    p.push(version);
    p.extend_from_slice(&flags.to_be_bytes()[1..4]);
    p.extend_from_slice(body);
    bx(typ, &p)
}

/// `[size][type][child0][child1]…` container box.
fn container(typ: &[u8; 4], children: &[&[u8]]) -> Vec<u8> {
    let total: usize = children.iter().map(|c| c.len()).sum();
    let mut payload = Vec::with_capacity(total);
    for c in children {
        payload.extend_from_slice(c);
    }
    bx(typ, &payload)
}

/// The 3×3 video transform matrix (unity), 9 × 16.16 fixed-point as big-endian u32.
const UNITY_MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn matrix_bytes() -> Vec<u8> {
    let mut v = Vec::with_capacity(36);
    for x in UNITY_MATRIX {
        v.extend_from_slice(&x.to_be_bytes());
    }
    v
}

fn default_ftyp() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"isom");
    body.extend_from_slice(&0x200u32.to_be_bytes());
    for brand in [b"isom", b"iso2", b"avc1", b"mp41"] {
        body.extend_from_slice(brand);
    }
    bx(b"ftyp", &body)
}

/// Assemble a one-track, one-sample MP4: copied `ftyp` + a synthesized `moov` describing a
/// single video sample (codec config copied verbatim from the source `stsd`) + an `mdat` of
/// just that keyframe's bytes. `dur` is the sample's duration in `timescale` units.
///
/// `pub(crate)`: this is a PURE muxer — a function of its arguments only, never of a source
/// container — so `flv` reuses it with an `stsd` it synthesizes from an FLV's AVC config.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_mini_mp4(
    src_ftyp: Option<&[u8]>,
    stsd: &[u8],
    desc_index: u32,
    dur: u64,
    timescale: u32,
    width: u16,
    height: u16,
    keyframe: &[u8],
) -> Vec<u8> {
    let ftyp = src_ftyp.map(|f| f.to_vec()).unwrap_or_else(default_ftyp);
    let dur32 = dur.min(u32::MAX as u64) as u32;
    let timescale = timescale.max(1);

    // mvhd (v0)
    let mut mvhd_body = Vec::new();
    mvhd_body.extend_from_slice(&0u32.to_be_bytes()); // creation
    mvhd_body.extend_from_slice(&0u32.to_be_bytes()); // modification
    mvhd_body.extend_from_slice(&timescale.to_be_bytes());
    mvhd_body.extend_from_slice(&dur32.to_be_bytes());
    mvhd_body.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
    mvhd_body.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
    mvhd_body.extend_from_slice(&0u16.to_be_bytes()); // reserved
    mvhd_body.extend_from_slice(&[0u8; 8]); // reserved
    mvhd_body.extend_from_slice(&matrix_bytes());
    mvhd_body.extend_from_slice(&[0u8; 24]); // pre_defined
    mvhd_body.extend_from_slice(&2u32.to_be_bytes()); // next_track_id
    let mvhd = fbx(b"mvhd", 0, 0, &mvhd_body);

    // tkhd (v0, enabled | in-movie | in-preview)
    let mut tkhd_body = Vec::new();
    tkhd_body.extend_from_slice(&0u32.to_be_bytes()); // creation
    tkhd_body.extend_from_slice(&0u32.to_be_bytes()); // modification
    tkhd_body.extend_from_slice(&1u32.to_be_bytes()); // track_id
    tkhd_body.extend_from_slice(&0u32.to_be_bytes()); // reserved
    tkhd_body.extend_from_slice(&dur32.to_be_bytes());
    tkhd_body.extend_from_slice(&[0u8; 8]); // reserved
    tkhd_body.extend_from_slice(&0u16.to_be_bytes()); // layer
    tkhd_body.extend_from_slice(&0u16.to_be_bytes()); // alternate_group
    tkhd_body.extend_from_slice(&0u16.to_be_bytes()); // volume (video = 0)
    tkhd_body.extend_from_slice(&0u16.to_be_bytes()); // reserved
    tkhd_body.extend_from_slice(&matrix_bytes());
    tkhd_body.extend_from_slice(&((width as u32) << 16).to_be_bytes()); // 16.16
    tkhd_body.extend_from_slice(&((height as u32) << 16).to_be_bytes());
    let tkhd = fbx(b"tkhd", 0, 0x0000_0007, &tkhd_body);

    // mdhd (v0)
    let mut mdhd_body = Vec::new();
    mdhd_body.extend_from_slice(&0u32.to_be_bytes()); // creation
    mdhd_body.extend_from_slice(&0u32.to_be_bytes()); // modification
    mdhd_body.extend_from_slice(&timescale.to_be_bytes());
    mdhd_body.extend_from_slice(&dur32.to_be_bytes());
    mdhd_body.extend_from_slice(&0x55C4u16.to_be_bytes()); // language 'und'
    mdhd_body.extend_from_slice(&0u16.to_be_bytes()); // pre_defined
    let mdhd = fbx(b"mdhd", 0, 0, &mdhd_body);

    // hdlr (vide)
    let mut hdlr_body = Vec::new();
    hdlr_body.extend_from_slice(&0u32.to_be_bytes()); // pre_defined
    hdlr_body.extend_from_slice(b"vide"); // handler_type
    hdlr_body.extend_from_slice(&[0u8; 12]); // reserved
    hdlr_body.extend_from_slice(b"VideoHandler\0");
    let hdlr = fbx(b"hdlr", 0, 0, &hdlr_body);

    // vmhd / dinf(dref(url ))
    let vmhd = fbx(b"vmhd", 0, 0x0000_0001, &[0u8; 8]);
    let url = fbx(b"url ", 0, 0x0000_0001, &[]); // self-contained
    let mut dref_body = Vec::new();
    dref_body.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    dref_body.extend_from_slice(&url);
    let dref = fbx(b"dref", 0, 0, &dref_body);
    let dinf = container(b"dinf", &[&dref]);

    // stbl children describing exactly one sample
    let stsd = stsd.to_vec(); // verbatim copy (codec config)
    let stts = fbx(b"stts", 0, 0, &concat32(&[1, 1, dur32])); // 1 entry: count=1, delta=dur
    let stss = fbx(b"stss", 0, 0, &concat32(&[1, 1])); // entry_count=1, sample 1 is sync
    let stsc = fbx(b"stsc", 0, 0, &concat32(&[1, 1, 1, desc_index])); // chunk1, 1 sample, desc
    let stsz = fbx(b"stsz", 0, 0, &concat32(&[0, 1, keyframe.len() as u32])); // size table, 1 sample
    let stco = fbx(b"stco", 0, 0, &concat32(&[1, 0])); // entry_count=1, offset patched below

    let stbl = container(b"stbl", &[&stsd, &stts, &stss, &stsc, &stsz, &stco]);
    let minf = container(b"minf", &[&vmhd, &dinf, &stbl]);
    let mdia = container(b"mdia", &[&mdhd, &hdlr, &minf]);
    let trak = container(b"trak", &[&tkhd, &mdia]);
    let moov = container(b"moov", &[&mvhd, &trak]);

    // The single chunk offset must point at the keyframe bytes in the final file. Its 4-byte
    // value sits at a fixed position (box lengths are independent of the value), so we compute
    // that position from the box sizes rather than scanning — robust against any "stco" bytes
    // that might coincidentally appear inside the copied codec config.
    let stco_field = ftyp.len()
        + 8 + mvhd.len()                                   // into moov → start of trak
        + 8 + tkhd.len()                                   // into trak → start of mdia
        + 8 + mdhd.len() + hdlr.len()                      // into mdia → start of minf
        + 8 + vmhd.len() + dinf.len()                      // into minf → start of stbl
        + 8 + stsd.len() + stts.len() + stss.len() + stsc.len() + stsz.len() // → start of stco
        + 16; // stco header(8) + version/flags(4) + entry_count(4)
    let mdat_data_off = (ftyp.len() + moov.len() + 8) as u32; // ftyp + moov + mdat header

    let mut out = Vec::with_capacity(ftyp.len() + moov.len() + 8 + keyframe.len());
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    out[stco_field..stco_field + 4].copy_from_slice(&mdat_data_off.to_be_bytes());
    out.extend_from_slice(&bx(b"mdat", keyframe));
    out
}

/// Concatenate big-endian u32s into a body (for the trivially-shaped sample-table boxes).
fn concat32(vals: &[u32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(vals.len() * 4);
    for &x in vals {
        v.extend_from_slice(&x.to_be_bytes());
    }
    v
}

/// Read exactly `buf.len()` bytes at absolute `off`, looping over short reads.
/// `pub(crate)` for `flv`'s tag walk.
pub(crate) fn read_exact_at<R: Read + Seek>(r: &mut R, off: u64, buf: &mut [u8]) -> Option<()> {
    r.seek(SeekFrom::Start(off)).ok()?;
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => return None,
            Ok(n) => filled += n,
            Err(_) => return None,
        }
    }
    Some(())
}

#[cfg(test)]
mod tests;
