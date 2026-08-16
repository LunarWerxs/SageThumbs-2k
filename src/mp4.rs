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
/// mini-MP4 bytes for [`crate::video::frame_from_bytes`], or `None` if the source isn't a
/// parseable ISO-BMFF with an indexed video track (caller falls back to the prefix path).
pub fn keyframe_mini_mp4<R: Read + Seek>(r: &mut R, fraction: f64) -> Option<Vec<u8>> {
    let (total, ftyp, moov) = scan_top_level(r)?;

    // --- Locate the video track's sample tables inside the moov ------------------------------
    let mdia_body = video_mdia(box_body(&moov))?;
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

    Some(build_mini_mp4(
        ftyp.as_deref(),
        stsd,
        desc_index,
        frame_delta.max(1),
        media_timescale,
        width,
        height,
        &keyframe,
    ))
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
        let mut hdr = [0u8; 8];
        if read_exact_at(r, pos, &mut hdr).is_none() {
            break;
        }
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
        let Some((full, _header_len)) =
            crate::container::boxhdr::decode_box_size(size32, extended, pos, total)
        else {
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
    for (typ, trak) in boxes(moov_body) {
        if &typ != b"trak" {
            continue;
        }
        // `continue`, not `?`: a trak with no `mdia` is one BAD track, not the end of the
        // search. Propagating None here abandoned the whole moov on the first oddball trak
        // (editors emit hint/metadata/placeholder traks), so a file whose video track came
        // second reported "no video track" — the smart index tier silently downgraded, and
        // doctor told the user the codec was unidentifiable in a file that plainly has one.
        let Some(mdia) = find(box_body(trak), b"mdia") else {
            continue;
        };
        let mdia_body = box_body(mdia);
        let hdlr = match find(mdia_body, b"hdlr") {
            Some(h) => full_box_body(h),
            None => continue,
        };
        // hdlr: pre_defined(4) handler_type(4) … — 'vide' marks the video track.
        if hdlr.get(4..8) == Some(b"vide") {
            return Some(mdia_body);
        }
    }
    None
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

/// The sample-entry fourcc of the video track's first `stsd` entry (`avc1`, `hvc1`, `av01`,
/// …), for the doctor's codec diagnosis. Reads the `ftyp` gate + `moov` only; `None` for
/// non-ISO-BMFF sources or video-less files.
pub fn video_codec_fourcc<R: Read + Seek>(r: &mut R) -> Option<[u8; 4]> {
    let (_, _, moov) = scan_top_level(r)?;
    let mdia_body = video_mdia(box_body(&moov))?;
    let minf = find(mdia_body, b"minf")?;
    let stbl = box_body(find(box_body(minf), b"stbl")?);
    let stsd = find(stbl, b"stsd")?;
    // stsd: header(8) version+flags(4) entry_count(4) | entry: size(4) type(4) …
    let t = stsd.get(20..24)?;
    Some([t[0], t[1], t[2], t[3]])
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
fn box_body(full: &[u8]) -> &[u8] {
    let size32 = u32::from_be_bytes([full[0], full[1], full[2], full[3]]);
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

/// Walk `stts` (time-to-sample) to find the decoding-order sample at `fraction` of the total
/// running time. Returns `(sample_index_0based, that_sample's_delta)`. The total duration is
/// the sum of the per-run `count*delta`, so this is timescale-independent.
fn stts_target(p: &[u8], fraction: f64) -> Option<(u64, u64)> {
    let n = g32(p, 0)? as usize;
    if n == 0 {
        return None;
    }
    // Pass 1: total running time + total sample count.
    let mut total_time = 0u64;
    let mut total_samples = 0u64;
    for i in 0..n {
        let count = g32(p, 4 + i * 8)? as u64;
        let delta = g32(p, 8 + i * 8)? as u64;
        total_time = total_time.checked_add(count.checked_mul(delta)?)?;
        total_samples = total_samples.checked_add(count)?;
    }
    if total_time == 0 || total_samples == 0 {
        return None;
    }
    let target = (total_time as f64 * fraction.clamp(0.0, 0.95)) as u64;

    // Pass 2: locate the sample whose presentation window contains `target`.
    let mut sample = 0u64;
    let mut elapsed = 0u64;
    let mut last_delta = 1u64;
    for i in 0..n {
        let count = g32(p, 4 + i * 8)? as u64;
        let delta = g32(p, 8 + i * 8)? as u64;
        last_delta = delta.max(1);
        if delta != 0 {
            let run = count * delta;
            if elapsed + run > target {
                let into = (target - elapsed) / delta;
                return Some((sample + into, delta));
            }
            elapsed += run;
        }
        sample += count;
    }
    Some((total_samples - 1, last_delta)) // target past the end → clamp to the last sample
}

/// The sync sample (1-based) at or before `target` (1-based). `stss` is sorted ascending, so we
/// take the largest entry ≤ target, else the first. `None` stss ⇒ every sample is sync ⇒ use
/// `target` itself. A sync sample is an IDR/IRAP, independently decodable as a standalone frame.
fn nearest_sync(stss: Option<&[u8]>, target: u64) -> Option<u64> {
    let Some(stss) = stss else {
        // Box absent: per ISO-BMFF that means every sample is independently
        // decodable, so the target sample itself is fine to use directly.
        return Some(target);
    };
    let p = full_box_body(stss);
    let n = g32(p, 0)? as usize;
    if n == 0 {
        // Box PRESENT but empty is a distinct claim from "box absent": the file
        // is asserting there are no sync samples at all, so nothing here is
        // independently decodable. Returning `target` here (as the absent case
        // does) would build a mini-MP4 around a sample that may need earlier
        // frames to decode.
        return None;
    }
    // Scan every entry and keep the best `<= target`, rather than trusting the
    // table's claimed ascending order and stopping at the first overshoot: an
    // unsorted stss from a buggy muxer would otherwise silently pick the wrong
    // (e.g. frame-1) sync sample. `p` is already bounded by MOOV_MAX, so a full
    // scan is cheap.
    let mut best: Option<u64> = None;
    let mut first = None;
    for i in 0..n {
        let s = g32(p, 4 + i * 4)? as u64;
        if first.is_none() {
            first = Some(s);
        }
        if s <= target && best.is_none_or(|b| s > b) {
            best = Some(s);
        }
    }
    best.or(first)
}

/// `stsz` (uniform or per-sample) vs `stz2` (compact 4/8/16-bit) sample sizes. Holds the whole
/// box so a size lookup is a pure function of the chosen sample index.
enum SampleSizes<'a> {
    Stsz(&'a [u8]),
    Stz2(&'a [u8]),
}

impl SampleSizes<'_> {
    /// Byte size of sample `idx` (0-based), or `None` if out of range / unsupported field size.
    fn size_of(&self, idx: u64) -> Option<u64> {
        let idx = idx as usize;
        match self {
            SampleSizes::Stsz(full) => {
                let p = full_box_body(full);
                let uniform = g32(p, 0)?;
                let count = g32(p, 4)? as usize;
                if idx >= count {
                    return None;
                }
                if uniform != 0 {
                    Some(uniform as u64)
                } else {
                    g32(p, 8 + idx * 4).map(u64::from)
                }
            }
            SampleSizes::Stz2(full) => {
                let p = full_box_body(full);
                let field = *p.get(3)?; // 24 reserved bits then an 8-bit field_size
                let count = g32(p, 4)? as usize;
                if idx >= count {
                    return None;
                }
                match field {
                    16 => g16(p, 8 + idx * 2).map(u64::from),
                    8 => p.get(8 + idx).map(|&b| b as u64),
                    4 => {
                        let byte = *p.get(8 + idx / 2)?;
                        let nib = if idx.is_multiple_of(2) {
                            byte >> 4
                        } else {
                            byte & 0x0F
                        };
                        Some(nib as u64)
                    }
                    _ => None,
                }
            }
        }
    }
}

/// Resolve sample `target` (0-based) to its absolute file byte offset via `stsc`
/// (sample→chunk) + `stco`/`co64` (chunk→offset), summing the sizes of earlier samples sharing
/// its chunk. Returns `(byte_offset, sample_description_index)`.
fn sample_location(
    stsc: &[u8],
    (chunks, is64): (&[u8], bool),
    sizes: &SampleSizes,
    target: u64,
) -> Option<(u64, u32)> {
    let chunk_body = full_box_body(chunks);
    let num_chunks = g32(chunk_body, 0)? as u64;
    let n = g32(stsc, 0)? as usize;

    let mut first_sample_of_run = 0u64;
    let mut hit = None;
    for i in 0..n {
        let base = 4 + i * 12;
        let first_chunk = g32(stsc, base)? as u64; // 1-based
        let spc = g32(stsc, base + 4)? as u64; // samples per chunk
        let desc = g32(stsc, base + 8)?;
        if spc == 0 {
            return None;
        }
        let next_first = if i + 1 < n {
            g32(stsc, base + 12)? as u64
        } else {
            // checked_add, matching the module's own discipline for file-derived
            // arithmetic: harmless in practice (num_chunks is u32-bounded) but keeps
            // debug builds from trapping and this site consistent with its neighbors.
            num_chunks.checked_add(1)?
        };
        if next_first < first_chunk {
            return None;
        }
        let samples_in_run = (next_first - first_chunk).checked_mul(spc)?;
        if target < first_sample_of_run + samples_in_run {
            let into = target - first_sample_of_run;
            let chunk_in_run = into / spc;
            let chunk1 = first_chunk + chunk_in_run; // 1-based chunk holding `target`
            let first_sample_of_chunk = first_sample_of_run + chunk_in_run * spc;
            hit = Some((chunk1, first_sample_of_chunk, desc));
            break;
        }
        // checked_add: `samples_in_run` comes from a checked_mul above and can approach
        // u64::MAX across many stsc entries, unlike the other u32-bounded terms in this
        // function, so this accumulator is the one term that genuinely needs the guard.
        first_sample_of_run = first_sample_of_run.checked_add(samples_in_run)?;
    }
    let (chunk1, first_sample_of_chunk, desc) = hit?;
    if chunk1 == 0 || chunk1 > num_chunks {
        return None;
    }
    let cidx = (chunk1 - 1) as usize;
    let mut offset = if is64 {
        g64(chunk_body, 4 + cidx * 8)?
    } else {
        g32(chunk_body, 4 + cidx * 4)? as u64
    };
    // Walk the samples ahead of `target` inside its chunk to accumulate their sizes. This is the
    // one unbounded loop left in the index walk, and it is fully attacker-controlled: a single
    // ~8-byte `stts` entry with count = 0xFFFFFFFF pushes `target` into the billions, and with a
    // uniform `stsz` (O(1) `size_of`, no backing array to make the file big) a few-hundred-byte
    // file buys billions of iterations. That is a compute-bound multi-second-to-minutes hang in the
    // thumbnail host, which is exactly the hang class this module exists to avoid. No real chunk
    // holds anywhere near this many samples, so a cap costs nothing and turns the attack into a
    // "can't read it" fallback.
    const MAX_SAMPLE_WALK: u64 = 100_000;
    if target.saturating_sub(first_sample_of_chunk) > MAX_SAMPLE_WALK {
        return None;
    }
    let mut s = first_sample_of_chunk;
    while s < target {
        offset = offset.checked_add(sizes.size_of(s)?)?;
        s += 1;
    }
    Some((offset, if desc == 0 { 1 } else { desc }))
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
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Seek, SeekFrom};
    use std::path::Path;

    /// Wraps a `Cursor` and counts `seek()` calls, so a test can prove a walk
    /// stopped early instead of just checking its (possibly coincidentally
    /// identical) final answer.
    struct CountingReader<'a> {
        inner: Cursor<&'a [u8]>,
        seeks: usize,
    }
    impl Read for CountingReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.inner.read(buf)
        }
    }
    impl Seek for CountingReader<'_> {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.seeks += 1;
            self.inner.seek(pos)
        }
    }

    /// Validate the box-navigation primitives on a hand-built nested structure.
    #[test]
    fn box_walk_finds_nested_children() {
        let leaf = fbx(b"stss", 0, 0, &concat32(&[1, 7]));
        let stbl = container(b"stbl", &[&leaf]);
        let minf = container(b"minf", &[&stbl]);
        let found_stbl = find(box_body(&minf), b"stbl").unwrap();
        let found_stss = find(box_body(found_stbl), b"stss").unwrap();
        // full_box_body skips ver/flags → entry_count then the sample number.
        assert_eq!(g32(full_box_body(found_stss), 0), Some(1));
        assert_eq!(g32(full_box_body(found_stss), 4), Some(7));
    }

    /// A hostile 64-bit box size must END the walk, not wrap the cursor into a panic.
    ///
    /// `size32 == 1` means "the real size is the u64 that follows", and that u64 is entirely
    /// file-controlled. With `u64::MAX` the old `pos + full` wrapped (release builds carry no
    /// overflow checks) to `pos - 1`, so `pos + full > buf.len()` was FALSE and the walk fell
    /// through to `&buf[pos..pos - 1]` — a start-after-end slice panic, i.e. an abort of
    /// whatever host was parsing, `explorer.exe` included.
    ///
    /// The second box is where it has to be tested: at `pos == 0` the wrap lands on
    /// `usize::MAX`, which the bounds check happens to catch. Only a non-zero cursor exposes it.
    #[test]
    fn a_wrapping_64bit_box_size_ends_the_walk_instead_of_panicking() {
        let mut buf = bx(b"ftyp", &[0u8; 8]); // a normal first box → cursor lands past 0
        let start = buf.len();
        assert!(start > 0);
        buf.extend_from_slice(&1u32.to_be_bytes()); // size32 == 1 → 64-bit extended form
        buf.extend_from_slice(b"moov");
        buf.extend_from_slice(&u64::MAX.to_be_bytes()); // the hostile length
        buf.extend_from_slice(&[0u8; 16]); // some payload, so the slice would be in-range-ish

        // Wrapping arithmetic is what the fix removed: prove the trap is still armed, so this
        // test cannot quietly stop measuring anything if the constant is ever changed.
        assert!(
            start.wrapping_add(u64::MAX as usize) < buf.len(),
            "the wrapped end must look in-bounds, or this fixture proves nothing"
        );

        let walked: Vec<[u8; 4]> = boxes(&buf).map(|(t, _)| t).collect();
        assert_eq!(
            walked,
            vec![*b"ftyp"],
            "the walk must stop AT the hostile box"
        );
        assert!(find(&buf, b"moov").is_none());
    }

    /// A `size32 == 1` box declares a 16-byte header (8 fixed + 8 extended), so its own claimed
    /// size can never legally be smaller than 16. The walker used to check the claimed size
    /// against a hardcoded `8` regardless of which header length was actually in play, so a box
    /// like this one — extended size 10, which only covers the *fixed* header — was silently
    /// accepted and yielded a box slice narrower than its own header (`box_body` would then read
    /// past the end of a 10-byte slice looking for the 16-byte header offset and get an empty
    /// body). The shared `decode_box_size` checks against the REAL header length (16 here), so
    /// this must now be rejected outright instead of walked past.
    #[test]
    fn a_size_one_box_shorter_than_its_own_16_byte_header_is_rejected() {
        let mut buf = bx(b"ftyp", &[0u8; 8]); // a normal first box
        buf.extend_from_slice(&1u32.to_be_bytes()); // size32 == 1 → extended form
        buf.extend_from_slice(b"moov");
        buf.extend_from_slice(&10u64.to_be_bytes()); // claimed size: shorter than the header

        let walked: Vec<[u8; 4]> = boxes(&buf).map(|(t, _)| t).collect();
        assert_eq!(
            walked,
            vec![*b"ftyp"],
            "a box claiming to be shorter than its own header must be rejected, not accepted"
        );
    }

    /// A trak with no `mdia` is one bad track, not the end of the moov. Editors emit
    /// hint/metadata/placeholder traks; when one led the file, the `?` here abandoned the
    /// whole search and a perfectly good video track that came second was reported absent
    /// (silent index-tier downgrade, plus doctor claiming the codec was unidentifiable).
    #[test]
    fn video_track_found_after_a_trak_with_no_mdia() {
        let mut hdlr_body = vec![0u8; 4]; // version+flags
        hdlr_body.extend_from_slice(&[0u8; 4]); // pre_defined
        hdlr_body.extend_from_slice(b"vide"); // handler_type
        hdlr_body.extend_from_slice(&[0u8; 12]);
        let hdlr = bx(b"hdlr", &hdlr_body);
        let mdia = container(b"mdia", &[&hdlr]);
        let good = container(b"trak", &[&mdia]);
        // A leading trak carrying only a tkhd — no mdia at all.
        let broken = container(b"trak", &[&bx(b"tkhd", &[0u8; 12])]);
        let moov = container(b"moov", &[&broken, &good]);

        assert!(
            video_mdia(box_body(&moov)).is_some(),
            "the video trak after an mdia-less trak must still be found"
        );
    }

    /// iTunes-style cover art: moov ▸ udta ▸ meta ▸ ilst ▸ covr ▸ data, with the 8-byte
    /// type-indicator/locale header stripped. `meta` is a FULL box here.
    #[test]
    fn cover_art_read_from_the_covr_atom() {
        const JPEG: &[u8] = b"\xFF\xD8\xFFrest-of-the-jpeg";
        let mut data_body = vec![0u8; 8]; // type indicator (13 = JPEG) + locale
        data_body[3] = 13;
        data_body.extend_from_slice(JPEG);
        let data = bx(b"data", &data_body);
        let covr = container(b"covr", &[&data]);
        let ilst = container(b"ilst", &[&covr]);
        // meta as a full box: version+flags then children.
        let meta = fbx(b"meta", 0, 0, &ilst);
        let udta = container(b"udta", &[&meta]);
        let moov = container(b"moov", &[&udta]);
        let mut file = bx(b"ftyp", b"isomisom");
        file.extend_from_slice(&moov);

        assert_eq!(
            cover_art(&mut Cursor::new(&file)).as_deref(),
            Some(JPEG),
            "the covr payload past the 8-byte data header"
        );

        // A QuickTime-style `meta` with NO version/flags must still resolve.
        let meta_plain = container(b"meta", &[&ilst]);
        let udta2 = container(b"udta", &[&meta_plain]);
        let moov2 = container(b"moov", &[&udta2]);
        let mut file2 = bx(b"ftyp", b"isomisom");
        file2.extend_from_slice(&moov2);
        assert_eq!(cover_art(&mut Cursor::new(&file2)).as_deref(), Some(JPEG));
    }

    /// A `covr` whose payload is not actually an image must be declined, so the caller
    /// falls through to the real decode tiers instead of being handed junk as "the cover".
    #[test]
    fn cover_art_declines_a_non_image_payload() {
        let mut data_body = vec![0u8; 8];
        data_body.extend_from_slice(b"this is not an image at all");
        let file = {
            let data = bx(b"data", &data_body);
            let covr = container(b"covr", &[&data]);
            let ilst = container(b"ilst", &[&covr]);
            let meta = fbx(b"meta", 0, 0, &ilst);
            let moov = container(b"moov", &[&container(b"udta", &[&meta])]);
            let mut f = bx(b"ftyp", b"isomisom");
            f.extend_from_slice(&moov);
            f
        };
        assert_eq!(cover_art(&mut Cursor::new(&file)), None);
        // And a file with no udta at all is simply None, not a panic.
        let bare = {
            let mut f = bx(b"ftyp", b"isomisom");
            f.extend_from_slice(&container(b"moov", &[&bx(b"mvhd", &[0u8; 32])]));
            f
        };
        assert_eq!(cover_art(&mut Cursor::new(&bare)), None);
    }

    /// 30 % of a uniform-cadence track should land on the sample nearest that time.
    #[test]
    fn stts_maps_fraction_to_sample() {
        // 100 samples, each 1000 ticks → total 100_000; 30 % → tick 30_000 → sample 30.
        let stts = fbx(b"stts", 0, 0, &concat32(&[1, 100, 1000]));
        let (sample, delta) = stts_target(full_box_body(&stts), 0.30).unwrap();
        assert_eq!(sample, 30);
        assert_eq!(delta, 1000);
    }

    /// nearest_sync takes the keyframe at or before the target; None stss ⇒ target itself.
    #[test]
    fn sync_sample_selection() {
        let stss = fbx(b"stss", 0, 0, &concat32(&[3, 1, 31, 61])); // sync at 1,31,61
        assert_eq!(nearest_sync(Some(&stss), 31), Some(31));
        assert_eq!(nearest_sync(Some(&stss), 45), Some(31)); // at-or-before
        assert_eq!(nearest_sync(Some(&stss), 70), Some(61));
        assert_eq!(nearest_sync(Some(&stss), 1), Some(1));
        assert_eq!(nearest_sync(None, 42), Some(42)); // all samples sync
    }

    /// stsc/stco offset resolution: 2 chunks × 2 samples, uniform 10-byte samples.
    #[test]
    fn sample_offset_resolution() {
        // chunk1 @ 1000, chunk2 @ 2000; each holds 2 samples of 10 bytes.
        let stsc = fbx(b"stsc", 0, 0, &concat32(&[1, 1, 2, 1])); // one run: first_chunk=1, spc=2, desc=1
        let stco = fbx(b"stco", 0, 0, &concat32(&[2, 1000, 2000]));
        let stsz = fbx(b"stsz", 0, 0, &concat32(&[10, 4])); // uniform 10 bytes, 4 samples
        let sizes = SampleSizes::Stsz(&stsz);
        // sample 0 → chunk1 + 0 = 1000; sample1 → 1010; sample2 → chunk2 = 2000; sample3 → 2010
        let cases = [(0u64, 1000u64), (1, 1010), (2, 2000), (3, 2010)];
        for (s, want) in cases {
            let (off, desc) =
                sample_location(full_box_body(&stsc), (&stco, false), &sizes, s).unwrap();
            assert_eq!(off, want, "sample {s}");
            assert_eq!(desc, 1);
        }
    }

    /// stz2 compact sizes (8- and 16-bit fields).
    #[test]
    fn stz2_sizes() {
        let mut body8 = Vec::new();
        body8.push(0); // reserved
        body8.extend_from_slice(&[0, 0]); // reserved
        body8.push(8); // field_size
        body8.extend_from_slice(&3u32.to_be_bytes()); // sample_count
        body8.extend_from_slice(&[11, 22, 33]);
        let stz2 = fbx(b"stz2", 0, 0, &body8);
        let sizes = SampleSizes::Stz2(&stz2);
        assert_eq!(sizes.size_of(0), Some(11));
        assert_eq!(sizes.size_of(2), Some(33));
        assert_eq!(sizes.size_of(3), None);
    }

    /// End-to-end: parse a real MP4 (the corpus `sample.mp4`, or `ST2K_TEST_VIDEO` if set)
    /// into a one-keyframe mini-MP4 and decode it through Media Foundation. Skipped where no
    /// sample is available (e.g. CI), so `cargo test` stays green without committing a video
    /// fixture.
    #[test]
    fn real_mp4_round_trips_through_mediafoundation() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let candidates = [
            std::env::var("ST2K_TEST_VIDEO").ok(),
            Some(
                base.join("test-corpus-real")
                    .join("sample.mp4")
                    .to_string_lossy()
                    .into_owned(),
            ),
            Some(
                base.join("test-corpus")
                    .join("sample.mp4")
                    .to_string_lossy()
                    .into_owned(),
            ),
        ];
        let Some(path) = candidates
            .into_iter()
            .flatten()
            .find(|p| Path::new(p).is_file())
        else {
            eprintln!("real_mp4_round_trips: no sample video found — skipping");
            return;
        };

        let bytes = std::fs::read(&path).expect("read sample video");
        let mini = keyframe_mini_mp4(&mut Cursor::new(&bytes), 0.30)
            .expect("build mini-mp4 from real sample");
        assert!(
            mini.len() < bytes.len().max(2 * 1024 * 1024),
            "mini-mp4 should be small ({} bytes from {} source)",
            mini.len(),
            bytes.len()
        );
        // The synthesized container must start ftyp…moov…mdat.
        assert_eq!(&mini[4..8], b"ftyp");
        assert!(mini.windows(4).any(|w| w == b"moov"));
        assert!(mini.windows(4).any(|w| w == b"mdat"));

        let frame = crate::video::frame_from_bytes(&mini)
            .expect("Media Foundation should decode the mini-mp4 keyframe");
        assert!(frame.width() > 0 && frame.height() > 0);
        eprintln!(
            "real_mp4_round_trips: {} → mini {} bytes → frame {}x{}",
            path,
            mini.len(),
            frame.width(),
            frame.height()
        );
    }

    /// A030: `scan_top_level`'s loop used to be bounded only by total file size, so a
    /// file built from many tiny top-level boxes (no `moov` anywhere) drove one
    /// Seek+Read pair per box. Build more boxes than `MAX_TOP_LEVEL_BOXES` and prove
    /// the walk stops at the cap instead of visiting every one of them: checks the
    /// seek COUNT, not just the final `None`, since "no moov" returns `None` either way.
    #[test]
    fn scan_top_level_bails_after_max_top_level_boxes() {
        let mut buf = bx(b"ftyp", &[0u8; 8]);
        let tiny_box_count = MAX_TOP_LEVEL_BOXES as usize + 900;
        for _ in 0..tiny_box_count {
            buf.extend_from_slice(&bx(b"free", &[]));
        }
        let mut r = CountingReader {
            inner: Cursor::new(buf.as_slice()),
            seeks: 0,
        };
        assert_eq!(
            scan_top_level(&mut r),
            None,
            "no moov present anywhere, so this must fail either way"
        );
        assert!(
            r.seeks <= MAX_TOP_LEVEL_BOXES as usize + 2,
            "must bail at the box cap instead of walking all {tiny_box_count} boxes (seeks = {})",
            r.seeks
        );
    }

    /// A233: `stss` PRESENT but with sample_count == 0 means there are NO sync
    /// samples, which is a different claim than `stss` ABSENT (every sample is
    /// sync). Both used to return `Some(target)`, silently building a mini-MP4
    /// around a sample that may not be independently decodable.
    #[test]
    fn nearest_sync_declines_a_present_but_empty_stss() {
        let empty_stss = fbx(b"stss", 0, 0, &concat32(&[0])); // sample_count = 0
        assert_eq!(nearest_sync(Some(&empty_stss), 42), None);
        // The genuinely-absent case is unaffected: still "every sample is sync".
        assert_eq!(nearest_sync(None, 42), Some(42));
    }

    /// A234: the old scan trusted stss's claimed ascending order and broke at the
    /// first overshoot, so an unsorted table (a buggy muxer) could silently return
    /// the wrong sync sample instead of the true nearest one <= target.
    #[test]
    fn nearest_sync_finds_the_true_nearest_in_an_unsorted_stss() {
        // Deliberately out of order: 61 appears before 31, but 31 is the nearer
        // sync sample <= 45. A break-on-first-overshoot scan sees 61 > 45 first
        // and stops, missing 31 (and returning the wrong answer) entirely.
        let stss = fbx(b"stss", 0, 0, &concat32(&[3, 61, 31, 1]));
        assert_eq!(nearest_sync(Some(&stss), 45), Some(31));
    }
}
