//! Smart targeted read for Matroska / WebM video thumbnails — the EBML analog of
//! [`crate::mp4`]. Build a tiny self-contained `.mkv` holding one Cluster (the keyframe
//! nearest ~30 % of the running time) by reading the file's own **Cues** index, so the
//! thumbnail is a representative mid-video frame instead of the intro / a fade-in.
//!
//! Why this is needed separately from `mp4`: Matroska is an EBML container (no `moov`), so
//! the MP4 path's `ftyp` gate rejects it and it would otherwise fall to the bounded 64 MB
//! head-prefix tier — which only reaches the first few seconds. Why it's *fast*: we read the
//! header (EBML + Info + Tracks, a few KB), the Cues index (a few MB), and the one Cluster at
//! 30 % (single-digit MB) — one seek + small reads, never streaming the multi-GB original.
//!
//! Layout in the wild (verified on a real 2.5 GB HEVC mkv): EBML ▸ Segment ▸ { SeekHead, Info,
//! Tracks, Cluster×N, Cues-at-end }. The front-of-segment `SeekHead` points to the trailing
//! `Cues`, so we never walk the clusters. We then mux: copied EBML header + a fresh Segment
//! containing the copied Info (Duration zeroed) + copied Tracks (codec config) + the one copied
//! Cluster (its Timecode zeroed so the clip starts at t=0 and the decoder grabs its keyframe).
//!
//! Best-effort: a file with no Cues (or `SeekHead`), an unknown-size Cluster, or a layout we
//! can't map returns `None` and the caller falls back to the head-prefix tier — never worse.

use std::io::{Read, Seek, SeekFrom};

// EBML / Matroska element IDs (full IDs incl. the length-marker, as a big-endian integer).
const ID_EBML: u64 = 0x1A45_DFA3;
const ID_SEGMENT: u64 = 0x1853_8067;
const ID_SEEKHEAD: u64 = 0x114D_9B74;
const ID_SEEK: u64 = 0x4DBB;
const ID_SEEK_ID: u64 = 0x53AB;
const ID_SEEK_POSITION: u64 = 0x53AC;
const ID_INFO: u64 = 0x1549_A966;
const ID_TIMECODE_SCALE: u64 = 0x2AD7B1;
const ID_DURATION: u64 = 0x4489;
const ID_TRACKS: u64 = 0x1654_AE6B;
const ID_TRACK_ENTRY: u64 = 0xAE;
const ID_TRACK_NUMBER: u64 = 0xD7;
const ID_TRACK_TYPE: u64 = 0x83;
const ID_CUES: u64 = 0x1C53_BB6B;
const ID_CUE_POINT: u64 = 0xBB;
const ID_CUE_TIME: u64 = 0xB3;
const ID_CUE_TRACK_POSITIONS: u64 = 0xB7;
const ID_CUE_TRACK: u64 = 0xF7;
const ID_CUE_CLUSTER_POSITION: u64 = 0xF1;
const ID_CLUSTER: u64 = 0x1F43_B675;
const ID_CLUSTER_TIMECODE: u64 = 0xE7;
const ID_CODEC_ID: u64 = 0x86;
const ID_ATTACHMENTS: u64 = 0x1941_A469;
const ID_ATTACHED_FILE: u64 = 0x61A7;
const ID_FILE_NAME: u64 = 0x466E;
const ID_FILE_MIME: u64 = 0x4660;
const ID_FILE_DATA: u64 = 0x465C;

const TRACK_TYPE_VIDEO: u64 = 1;

// Sanity caps on the bounded elements we pull into memory.
const META_MAX: u64 = 8 * 1024 * 1024; // EBML header / Info / Tracks
const CUES_MAX: u64 = 32 * 1024 * 1024; // the index
const CLUSTER_MAX: u64 = 96 * 1024 * 1024; // one cluster (≤ a few seconds of 4K)
const ATTACH_MAX: u64 = 64 * 1024 * 1024; // all attachments (cover art + subtitle fonts)

/// Where a Matroska file's Segment metadata sits: the verbatim EBML header plus absolute
/// positions of the top-level children the readers below need — resolved by the
/// front-of-segment walk with the SeekHead filling in whatever sits past the first Cluster
/// (Cues and, sometimes, Attachments live at the file's end).
struct SegmentMap {
    ebml: Vec<u8>,
    /// Absolute file size (bounds checks) and Segment data start (Positions are relative to it).
    total: u64,
    seg_data: u64,
    info: Option<u64>,
    tracks: Option<u64>,
    cues: Option<u64>,
    attachments: Option<u64>,
}

/// Parse the EBML + Segment headers and locate the metadata children. `None` if `r` isn't
/// Matroska/WebM at all — every public reader in this module gates through this, so each
/// self-rejects other containers cheaply.
fn segment_map<R: Read + Seek>(r: &mut R) -> Option<SegmentMap> {
    let total = r.seek(SeekFrom::End(0)).ok()?;

    // EBML header (copied verbatim) must be the first element.
    let (id, size, hlen, unknown) = header_at(r, 0)?;
    if id != ID_EBML || unknown {
        return None;
    }
    let ebml_len = hlen.checked_add(size)?;
    if ebml_len > META_MAX {
        return None;
    }
    let mut ebml = vec![0u8; ebml_len as usize];
    read_exact_at(r, 0, &mut ebml)?;

    // Segment.
    let (sid, ssize, shlen, sunk) = header_at(r, ebml_len)?;
    if sid != ID_SEGMENT {
        return None;
    }
    let seg_data = ebml_len + shlen; // Segment Positions are relative to here
    let seg_end = if sunk {
        total
    } else {
        (seg_data + ssize).min(total)
    };

    // Front-of-segment walk: capture SeekHead/Info/Tracks (and Cues/Attachments if they
    // happen to be up front), stopping at the first Cluster — we never scan the cluster body.
    let mut seekhead: Option<Vec<u8>> = None;
    let mut info_pos = None;
    let mut tracks_pos = None;
    let mut cues_pos = None;
    let mut attach_pos = None;
    let mut p = seg_data;
    for _ in 0..64 {
        if p + 2 > seg_end {
            break;
        }
        let (eid, esize, ehlen, eunk) = header_at(r, p)?;
        match eid {
            ID_SEEKHEAD if esize <= META_MAX => seekhead = read_full_at(r, p + ehlen, esize),
            ID_INFO => info_pos = Some(p),
            ID_TRACKS => tracks_pos = Some(p),
            ID_CUES => cues_pos = Some(p),
            ID_ATTACHMENTS => attach_pos = Some(p),
            ID_CLUSTER => break,
            _ => {}
        }
        if eunk {
            break; // can't skip an unknown-size element
        }
        p = p.checked_add(ehlen + esize)?;
        // No early exit on "found everything": Attachments routinely sit AFTER Cues in a
        // cues-up-front layout, and a file without a (complete) SeekHead would then lose
        // its cover art to the shortcut. The walk stops at the first Cluster anyway, so
        // finishing it costs a handful of header reads, bounded by the iteration cap.
    }

    // Resolve anything still missing via the SeekHead (Cues are typically at the file's end).
    if let Some(sh) = &seekhead {
        if cues_pos.is_none() {
            cues_pos = seek_lookup(sh, ID_CUES).map(|rel| seg_data + rel);
        }
        if info_pos.is_none() {
            info_pos = seek_lookup(sh, ID_INFO).map(|rel| seg_data + rel);
        }
        if tracks_pos.is_none() {
            tracks_pos = seek_lookup(sh, ID_TRACKS).map(|rel| seg_data + rel);
        }
        if attach_pos.is_none() {
            attach_pos = seek_lookup(sh, ID_ATTACHMENTS).map(|rel| seg_data + rel);
        }
    }

    Some(SegmentMap {
        ebml,
        total,
        seg_data,
        info: info_pos,
        tracks: tracks_pos,
        cues: cues_pos,
        attachments: attach_pos,
    })
}

/// Build a one-cluster mini-MKV for the keyframe nearest `fraction` of the running time, for
/// [`crate::video::frame_from_bytes`]. `None` if the source isn't a Cues-indexed Matroska/WebM
/// (caller falls back to the bounded head-prefix tier).
pub fn keyframe_mini_mkv<R: Read + Seek>(r: &mut R, fraction: f64) -> Option<Vec<u8>> {
    let map = segment_map(r)?;
    let (total, seg_data, ebml) = (map.total, map.seg_data, map.ebml);

    let (_, info_hlen, mut info) = read_element_full(r, map.info?, META_MAX, ID_INFO)?;
    let (_, tracks_hlen, tracks) = read_element_full(r, map.tracks?, META_MAX, ID_TRACKS)?;
    let (_, cues_hlen, cues) = read_element_full(r, map.cues?, CUES_MAX, ID_CUES)?;

    // Pick the cluster: video track number, the Cue list, then the cue nearest `fraction`.
    let video_track = video_track_number(&tracks[tracks_hlen..]);
    let (duration, _timescale) = info_duration(&info[info_hlen..]);
    let cues_list = cue_points(&cues[cues_hlen..], video_track);
    if cues_list.is_empty() {
        return None;
    }
    let frac = fraction.clamp(0.0, 0.95);
    let idx = match duration {
        Some(d) if d > 0.0 => {
            let target = (d * frac) as u64;
            // Largest cue at or before the target time (a keyframe at/just before 30%).
            let mut chosen = 0;
            for (i, (t, _)) in cues_list.iter().enumerate() {
                if *t <= target {
                    chosen = i;
                } else {
                    break;
                }
            }
            chosen
        }
        // No Duration → cues are ~evenly spaced, so index into the list by the fraction.
        _ => ((cues_list.len() as f64 * frac) as usize).min(cues_list.len() - 1),
    };
    let cluster_abs = seg_data.checked_add(cues_list[idx].1)?;
    if cluster_abs >= total {
        return None;
    }

    // Copy that one Cluster, then zero its Timecode so the mini-clip starts at t=0 (otherwise
    // `frame_from_bytes`'s near-the-head seek would land before the cluster's real timestamp
    // and grab nothing). Likewise zero Info's Duration so that seek computes ~0.
    let (_, cluster_hlen, mut cluster) =
        read_element_full(r, cluster_abs, CLUSTER_MAX, ID_CLUSTER)?;
    zero_child(&mut cluster, cluster_hlen, ID_CLUSTER_TIMECODE);
    zero_child(&mut info, info_hlen, ID_DURATION);

    Some(build_mini_mkv(&ebml, &info, &tracks, &cluster))
}

/// The CodecID of the first video track ("V_MPEGH/ISO/HEVC", "V_AV1", …), for the doctor's
/// codec diagnosis. Cheap: reads the EBML head plus the Tracks element only — a few KB —
/// and `None` for non-Matroska sources or video-less files.
pub fn video_codec_id<R: Read + Seek>(r: &mut R) -> Option<String> {
    let map = segment_map(r)?;
    let (_, hlen, tracks) = read_element_full(r, map.tracks?, META_MAX, ID_TRACKS)?;
    video_track_codec(&tracks[hlen..])
}

/// The attached cover image of a Matroska file: `cover.*` (the name the Matroska spec
/// blesses for exactly this), else the first `image/*` attachment. Library rips routinely
/// carry a poster this way, so when no frame can be decoded (usually a missing OS codec —
/// HEVC/AV1 ship as Store add-ons) the tile can still show the film instead of nothing.
pub fn attached_cover<R: Read + Seek>(r: &mut R) -> Option<Vec<u8>> {
    let map = segment_map(r)?;
    let attach_pos = map.attachments?;
    if attach_pos >= map.total {
        return None;
    }
    let (_, hlen, att) = read_element_full(r, attach_pos, ATTACH_MAX, ID_ATTACHMENTS)?;
    pick_cover(&att[hlen..])
}

/// Assemble: copied EBML header + a definite-size Segment wrapping the copied Info, Tracks, and
/// the one Cluster. No SeekHead/Cues — Media Foundation reads the three children sequentially.
fn build_mini_mkv(ebml: &[u8], info: &[u8], tracks: &[u8], cluster: &[u8]) -> Vec<u8> {
    let body = info.len() + tracks.len() + cluster.len();
    let mut out = Vec::with_capacity(ebml.len() + 12 + body);
    out.extend_from_slice(ebml);
    out.extend_from_slice(&ID_SEGMENT.to_be_bytes()[4..]); // 4-byte Segment ID
    out.extend_from_slice(&encode_vint(body as u64));
    out.extend_from_slice(info);
    out.extend_from_slice(tracks);
    out.extend_from_slice(cluster);
    out
}

// ---------------------------------------------------------------------------------------------
// Streaming element reads
// ---------------------------------------------------------------------------------------------

/// Parse the element header at absolute `pos`: `(id, data_size, header_len, size_is_unknown)`.
fn header_at<R: Read + Seek>(r: &mut R, pos: u64) -> Option<(u64, u64, u64, bool)> {
    r.seek(SeekFrom::Start(pos)).ok()?;
    let mut b = [0u8; 1];

    // Element ID: 1–4 bytes, value keeps the length-marker bit.
    r.read_exact(&mut b).ok()?;
    if b[0] == 0 {
        return None;
    }
    let id_len = b[0].leading_zeros() as usize + 1;
    if id_len > 4 {
        return None;
    }
    let mut id = b[0] as u64;
    for _ in 1..id_len {
        r.read_exact(&mut b).ok()?;
        id = (id << 8) | b[0] as u64;
    }

    // Size: 1–8 bytes, value strips the marker bit; all-ones data = unknown size.
    r.read_exact(&mut b).ok()?;
    if b[0] == 0 {
        return None;
    }
    let sz_len = b[0].leading_zeros() as usize + 1;
    if sz_len > 8 {
        return None;
    }
    // Widen before shifting: an 8-byte size vint (first byte 0x01 — ffmpeg writes the
    // Segment size this way routinely) needs `0xFF >> 8`, which overflows a u8 shift.
    // The u8 version panicked in debug and, worse, silently produced mask 0xFF in release —
    // a phantom 2^56 in every 8-byte size and unknown-size never detected.
    let mask = (0xFFu16 >> sz_len) as u8;
    let mut size = (b[0] & mask) as u64;
    let mut all_ones = (b[0] & mask) == mask;
    for _ in 1..sz_len {
        r.read_exact(&mut b).ok()?;
        size = (size << 8) | b[0] as u64;
        if b[0] != 0xFF {
            all_ones = false;
        }
    }
    Some((id, size, (id_len + sz_len) as u64, all_ones))
}

/// Read a whole element (header + data) at `pos`, verifying its id and bounding its size.
/// Returns `(id, header_len, full_element_bytes)`.
fn read_element_full<R: Read + Seek>(
    r: &mut R,
    pos: u64,
    cap: u64,
    want_id: u64,
) -> Option<(u64, usize, Vec<u8>)> {
    let (id, size, hlen, unknown) = header_at(r, pos)?;
    if id != want_id || unknown {
        return None;
    }
    let total = hlen.checked_add(size)?;
    if total > cap {
        return None;
    }
    let mut buf = vec![0u8; total as usize];
    read_exact_at(r, pos, &mut buf)?;
    Some((id, hlen as usize, buf))
}

fn read_full_at<R: Read + Seek>(r: &mut R, pos: u64, len: u64) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; len as usize];
    read_exact_at(r, pos, &mut buf)?;
    Some(buf)
}

fn read_exact_at<R: Read + Seek>(r: &mut R, off: u64, buf: &mut [u8]) -> Option<()> {
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

// ---------------------------------------------------------------------------------------------
// EBML slice parsing (over already-buffered elements)
// ---------------------------------------------------------------------------------------------

/// Iterate child elements of an in-memory element body, yielding `(id, data_offset, data)`
/// where `data_offset` is the child's data position within `buf`. Stops at the first malformed
/// or unknown-size child so a corrupt index can't loop or over-read.
fn children(buf: &[u8]) -> impl Iterator<Item = (u64, usize, &[u8])> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        let (id, id_len) = vint(buf, pos, 4)?;
        let (size, sz_len, unknown) = vint_size(buf, pos + id_len)?;
        if unknown {
            return None;
        }
        let dstart = pos + id_len + sz_len;
        let dend = dstart.checked_add(size as usize)?;
        if dend > buf.len() {
            return None;
        }
        let data = &buf[dstart..dend];
        pos = dend;
        Some((id, dstart, data))
    })
}

/// Parse an EBML ID vint at `pos` (≤ `max_len` bytes), keeping the marker bit. `(value, len)`.
fn vint(buf: &[u8], pos: usize, max_len: usize) -> Option<(u64, usize)> {
    let first = *buf.get(pos)?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    if len > max_len || pos + len > buf.len() {
        return None;
    }
    let mut v = 0u64;
    for i in 0..len {
        v = (v << 8) | buf[pos + i] as u64;
    }
    Some((v, len))
}

/// Parse an EBML size vint at `pos`, stripping the marker bit. `(value, len, is_unknown)`.
fn vint_size(buf: &[u8], pos: usize) -> Option<(u64, usize, bool)> {
    let first = *buf.get(pos)?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || pos + len > buf.len() {
        return None;
    }
    // Widened for the same reason as `header_at`: len == 8 must yield mask 0, not a panic
    // (debug) / 0xFF (release).
    let mask = (0xFFu16 >> len) as u8;
    let mut v = (first & mask) as u64;
    let mut all_ones = (first & mask) == mask;
    for i in 1..len {
        let b = buf[pos + i];
        v = (v << 8) | b as u64;
        if b != 0xFF {
            all_ones = false;
        }
    }
    Some((v, len, all_ones))
}

/// An EBML unsigned integer is a big-endian byte string (1–8 bytes).
fn ebml_uint(data: &[u8]) -> u64 {
    data.iter()
        .take(8)
        .fold(0u64, |acc, &b| (acc << 8) | b as u64)
}

/// An EBML float is 4- or 8-byte IEEE-754.
fn ebml_float(data: &[u8]) -> Option<f64> {
    match data.len() {
        4 => Some(f32::from_be_bytes(data.try_into().ok()?) as f64),
        8 => Some(f64::from_be_bytes(data.try_into().ok()?)),
        _ => None,
    }
}

/// TrackNumber of the first video TrackEntry (TrackType == 1), or `None`.
fn video_track_number(tracks_data: &[u8]) -> Option<u64> {
    for (id, _, entry) in children(tracks_data) {
        if id != ID_TRACK_ENTRY {
            continue;
        }
        let mut number = None;
        let mut ttype = None;
        for (cid, _, cd) in children(entry) {
            match cid {
                ID_TRACK_NUMBER => number = Some(ebml_uint(cd)),
                ID_TRACK_TYPE => ttype = Some(ebml_uint(cd)),
                _ => {}
            }
        }
        if ttype == Some(TRACK_TYPE_VIDEO) {
            return number;
        }
    }
    None
}

/// CodecID string of the first video TrackEntry (TrackType == 1), or `None`.
fn video_track_codec(tracks_data: &[u8]) -> Option<String> {
    for (id, _, entry) in children(tracks_data) {
        if id != ID_TRACK_ENTRY {
            continue;
        }
        let mut ttype = None;
        let mut codec = None;
        for (cid, _, cd) in children(entry) {
            match cid {
                ID_TRACK_TYPE => ttype = Some(ebml_uint(cd)),
                ID_CODEC_ID => {
                    codec = Some(
                        String::from_utf8_lossy(cd)
                            .trim_end_matches('\0')
                            .to_string(),
                    );
                }
                _ => {}
            }
        }
        if ttype == Some(TRACK_TYPE_VIDEO) {
            return codec;
        }
    }
    None
}

/// Pick the cover image out of an Attachments body: an AttachedFile named `cover.*` wins
/// outright (the spec's convention for the poster), else the first attachment that is an
/// image by mime type or file name. Fonts and other non-image attachments are skipped.
fn pick_cover(att_data: &[u8]) -> Option<Vec<u8>> {
    let mut fallback: Option<&[u8]> = None;
    for (id, _, af) in children(att_data) {
        if id != ID_ATTACHED_FILE {
            continue;
        }
        let mut name = None;
        let mut mime = None;
        let mut data: Option<&[u8]> = None;
        for (cid, _, cd) in children(af) {
            match cid {
                ID_FILE_NAME => name = Some(String::from_utf8_lossy(cd).to_lowercase()),
                ID_FILE_MIME => mime = Some(String::from_utf8_lossy(cd).to_lowercase()),
                ID_FILE_DATA => data = Some(cd),
                _ => {}
            }
        }
        let Some(d) = data.filter(|d| !d.is_empty()) else {
            continue;
        };
        let is_image = mime.as_deref().is_some_and(|m| m.starts_with("image/"))
            || name.as_deref().is_some_and(|n| {
                [".jpg", ".jpeg", ".png", ".webp"]
                    .iter()
                    .any(|e| n.ends_with(e))
            });
        if !is_image {
            continue;
        }
        if name.as_deref().is_some_and(|n| n.starts_with("cover")) {
            return Some(d.to_vec());
        }
        fallback.get_or_insert(d);
    }
    fallback.map(<[u8]>::to_vec)
}

/// `(Duration, TimecodeScale)` from an Info body. Duration is in TimecodeScale units — the same
/// unit as CueTime — so the two compare directly without converting to nanoseconds.
fn info_duration(info_data: &[u8]) -> (Option<f64>, u64) {
    let mut duration = None;
    let mut scale = 1_000_000u64;
    for (id, _, d) in children(info_data) {
        match id {
            ID_DURATION => duration = ebml_float(d),
            ID_TIMECODE_SCALE => scale = ebml_uint(d).max(1),
            _ => {}
        }
    }
    (duration, scale)
}

/// `(cue_time, cluster_segment_position)` for each CuePoint, preferring the video track's
/// CueTrackPositions (falling back to the first). Sorted ascending by time.
fn cue_points(cues_data: &[u8], video_track: Option<u64>) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for (id, _, cp) in children(cues_data) {
        if id != ID_CUE_POINT {
            continue;
        }
        let mut time = None;
        let mut pos = None; // video-track position
        let mut first_pos = None; // any-track fallback
        for (cid, _, cd) in children(cp) {
            match cid {
                ID_CUE_TIME => time = Some(ebml_uint(cd)),
                ID_CUE_TRACK_POSITIONS => {
                    let mut track = None;
                    let mut cpos = None;
                    for (tid, _, td) in children(cd) {
                        match tid {
                            ID_CUE_TRACK => track = Some(ebml_uint(td)),
                            ID_CUE_CLUSTER_POSITION => cpos = Some(ebml_uint(td)),
                            _ => {}
                        }
                    }
                    if let Some(cpos) = cpos {
                        first_pos.get_or_insert(cpos);
                        if (video_track.is_none() || track == video_track) && pos.is_none() {
                            pos = Some(cpos);
                        }
                    }
                }
                _ => {}
            }
        }
        if let (Some(t), Some(p)) = (time, pos.or(first_pos)) {
            out.push((t, p));
        }
    }
    out.sort_by_key(|&(t, _)| t);
    out
}

/// Zero the data bytes of the first `target` child within an element (`elem_hlen` = the element's
/// own header length). Used to neutralize the cluster Timecode / Info Duration in place without
/// changing any sizes.
fn zero_child(elem: &mut [u8], elem_hlen: usize, target: u64) {
    let range = children(&elem[elem_hlen..])
        .find(|(id, _, _)| *id == target)
        .map(|(_, off, d)| (elem_hlen + off, d.len()));
    if let Some((start, len)) = range {
        for b in &mut elem[start..start + len] {
            *b = 0;
        }
    }
}

/// Look up a top-level element's Segment Position by id in a SeekHead body.
fn seek_lookup(seekhead: &[u8], target_id: u64) -> Option<u64> {
    for (id, _, seek) in children(seekhead) {
        if id != ID_SEEK {
            continue;
        }
        let mut sid = None;
        let mut spos = None;
        for (cid, _, cd) in children(seek) {
            match cid {
                ID_SEEK_ID => sid = Some(ebml_uint(cd)),
                ID_SEEK_POSITION => spos = Some(ebml_uint(cd)),
                _ => {}
            }
        }
        if sid == Some(target_id) {
            return spos;
        }
    }
    None
}

/// Encode `n` as an EBML size vint (shortest length whose all-ones value isn't reserved).
fn encode_vint(n: u64) -> Vec<u8> {
    for len in 1u32..=8 {
        let cap = (1u64 << (7 * len)) - 1; // all-ones reserved for "unknown size"
        if n < cap {
            let mut v = vec![0u8; len as usize];
            let mut x = n;
            for i in (0..len as usize).rev() {
                v[i] = (x & 0xFF) as u8;
                x >>= 8;
            }
            v[0] |= 1u8 << (8 - len);
            return v;
        }
    }
    vec![0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::Path;

    #[test]
    fn vint_size_round_trips() {
        for n in [0u64, 1, 126, 127, 128, 16382, 16383, 100_000, 1 << 30] {
            let enc = encode_vint(n);
            let (val, len, unknown) = vint_size(&enc, 0).unwrap();
            assert_eq!(val, n, "value {n}");
            assert_eq!(len, enc.len());
            assert!(!unknown);
        }
    }

    #[test]
    fn children_iterates_and_offsets() {
        // Build: TimecodeScale(0x2AD7B1)=1000000, Duration(0x4489, f32)=2.0
        let mut info = Vec::new();
        info.extend_from_slice(&[0x2A, 0xD7, 0xB1]); // id (3 bytes)
        info.extend_from_slice(&encode_vint(3));
        info.extend_from_slice(&[0x0F, 0x42, 0x40]); // 1_000_000
        info.extend_from_slice(&[0x44, 0x89]); // Duration id
        info.extend_from_slice(&encode_vint(4));
        info.extend_from_slice(&2.0f32.to_be_bytes());
        let (dur, scale) = info_duration(&info);
        assert_eq!(scale, 1_000_000);
        assert_eq!(dur, Some(2.0));
        // zero_child should blank the Duration's 4 float bytes (wrap in a fake element header).
        let mut elem = vec![0u8; 4];
        elem.extend_from_slice(&info);
        zero_child(&mut elem, 4, ID_DURATION);
        assert_eq!(info_duration(&elem[4..]).0, Some(0.0));
    }

    #[test]
    fn cue_selection_prefers_video_track() {
        // Two cue points; track 1 = video, track 2 = audio, different cluster positions.
        let mut cues = Vec::new();
        for (time, vpos, apos) in [(0u64, 100u64, 50u64), (5000, 9000, 8000)] {
            let mut ctp_v = Vec::new();
            ctp_v.extend_from_slice(&[ID_CUE_TRACK as u8]);
            ctp_v.extend_from_slice(&encode_vint(1));
            ctp_v.push(1); // track 1
            ctp_v.extend_from_slice(&[ID_CUE_CLUSTER_POSITION as u8]);
            ctp_v.extend_from_slice(&encode_vint(2));
            ctp_v.extend_from_slice(&(vpos as u16).to_be_bytes());
            let mut ctp_a = Vec::new();
            ctp_a.extend_from_slice(&[ID_CUE_TRACK as u8]);
            ctp_a.extend_from_slice(&encode_vint(1));
            ctp_a.push(2); // track 2
            ctp_a.extend_from_slice(&[ID_CUE_CLUSTER_POSITION as u8]);
            ctp_a.extend_from_slice(&encode_vint(2));
            ctp_a.extend_from_slice(&(apos as u16).to_be_bytes());

            let mut cp = Vec::new();
            cp.extend_from_slice(&[ID_CUE_TIME as u8]);
            cp.extend_from_slice(&encode_vint(2));
            cp.extend_from_slice(&(time as u16).to_be_bytes());
            for ctp in [ctp_a, ctp_v] {
                // audio first, to prove we still pick the video position
                cp.extend_from_slice(&[ID_CUE_TRACK_POSITIONS as u8]);
                cp.extend_from_slice(&encode_vint(ctp.len() as u64));
                cp.extend_from_slice(&ctp);
            }
            cues.extend_from_slice(&[ID_CUE_POINT as u8]);
            cues.extend_from_slice(&encode_vint(cp.len() as u64));
            cues.extend_from_slice(&cp);
        }
        let list = cue_points(&cues, Some(1));
        assert_eq!(list, vec![(0, 100), (5000, 9000)]); // video-track positions, sorted
    }

    /// Emit one EBML element: id bytes (as stored in the `ID_*` constants) + size vint + data.
    fn elem(id: u64, data: &[u8]) -> Vec<u8> {
        let id_bytes = id.to_be_bytes();
        let start = id_bytes.iter().position(|&b| b != 0).unwrap();
        let mut out = Vec::new();
        out.extend_from_slice(&id_bytes[start..]);
        out.extend_from_slice(&encode_vint(data.len() as u64));
        out.extend_from_slice(data);
        out
    }

    /// The 8-byte size vint (first byte 0x01) is what ffmpeg writes for the Segment size in
    /// every muxed file. The u8 `0xFF >> 8` mask panicked in debug and mis-parsed in release
    /// (phantom 2^56 in the size, unknown-size never detected) — keep both shapes covered.
    #[test]
    fn eight_byte_size_vints_parse() {
        let known = [0x01u8, 0, 0, 0, 0, 0, 0, 0x2A];
        assert_eq!(vint_size(&known, 0), Some((42, 8, false)));
        let unknown = [0x01u8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(
            vint_size(&unknown, 0),
            Some((0x00FF_FFFF_FFFF_FFFF, 8, true))
        );

        // End-to-end through `header_at`: a Segment whose size is 8-byte encoded, the way
        // ffmpeg writes it, must still yield the Tracks walk (this panicked before the fix).
        let track = elem(
            ID_TRACK_ENTRY,
            &[
                elem(ID_TRACK_TYPE, &[TRACK_TYPE_VIDEO as u8]),
                elem(ID_CODEC_ID, b"V_MPEG4/ISO/AVC"),
            ]
            .concat(),
        );
        let body = elem(ID_TRACKS, &track);
        let mut file = elem(ID_EBML, &[0u8; 4]);
        file.extend_from_slice(&ID_SEGMENT.to_be_bytes()[4..]);
        file.push(0x01); // 8-byte size vint, value = body length
        file.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
        file.extend_from_slice(&body);
        assert_eq!(
            video_codec_id(&mut Cursor::new(&file)).as_deref(),
            Some("V_MPEG4/ISO/AVC")
        );
    }

    #[test]
    fn codec_id_and_attached_cover_from_synthetic_mkv() {
        let track_entry = [
            elem(ID_TRACK_NUMBER, &[1]),
            elem(ID_TRACK_TYPE, &[TRACK_TYPE_VIDEO as u8]),
            elem(ID_CODEC_ID, b"V_MPEGH/ISO/HEVC"),
        ]
        .concat();
        let tracks = elem(ID_TRACKS, &elem(ID_TRACK_ENTRY, &track_entry));
        // A font attachment FIRST — the cover must still win (fonts are the common company).
        let font = [
            elem(ID_FILE_NAME, b"subs.ttf"),
            elem(ID_FILE_MIME, b"application/x-truetype-font"),
            elem(ID_FILE_DATA, &[0xAA; 8]),
        ]
        .concat();
        let cover = [
            elem(ID_FILE_NAME, b"Cover.jpg"),
            elem(ID_FILE_MIME, b"image/jpeg"),
            elem(ID_FILE_DATA, b"JPEGDATA"),
        ]
        .concat();
        let attachments = elem(
            ID_ATTACHMENTS,
            &[
                elem(ID_ATTACHED_FILE, &font),
                elem(ID_ATTACHED_FILE, &cover),
            ]
            .concat(),
        );
        let mut file = elem(ID_EBML, &[0u8; 4]);
        file.extend_from_slice(&elem(ID_SEGMENT, &[tracks, attachments].concat()));

        let mut cur = Cursor::new(&file);
        assert_eq!(
            video_codec_id(&mut cur).as_deref(),
            Some("V_MPEGH/ISO/HEVC")
        );
        assert_eq!(
            attached_cover(&mut cur).as_deref(),
            Some(b"JPEGDATA".as_slice())
        );
    }

    /// Cues-up-front layout with NO SeekHead: Info, Tracks, Cues, Attachments, Cluster.
    /// The walk used to break as soon as info+tracks+cues were all found, skipping the
    /// Attachments element sitting right after Cues — losing the cover art of any file
    /// whose SeekHead is absent or doesn't list Attachments (mkvpropedit-appended covers).
    #[test]
    fn attachments_after_cues_survive_without_a_seekhead() {
        let tracks = elem(
            ID_TRACKS,
            &elem(
                ID_TRACK_ENTRY,
                &[
                    elem(ID_TRACK_TYPE, &[TRACK_TYPE_VIDEO as u8]),
                    elem(ID_CODEC_ID, b"V_MPEGH/ISO/HEVC"),
                ]
                .concat(),
            ),
        );
        let info = elem(ID_INFO, &elem(ID_TIMECODE_SCALE, &[0x0F, 0x42, 0x40]));
        let cues = elem(ID_CUES, &[]);
        let attachments = elem(
            ID_ATTACHMENTS,
            &elem(
                ID_ATTACHED_FILE,
                &[
                    elem(ID_FILE_NAME, b"cover.jpg"),
                    elem(ID_FILE_MIME, b"image/jpeg"),
                    elem(ID_FILE_DATA, b"JPEGDATA"),
                ]
                .concat(),
            ),
        );
        let cluster = elem(ID_CLUSTER, &elem(ID_CLUSTER_TIMECODE, &[0]));
        let body = [info, tracks, cues, attachments, cluster].concat();
        let mut file = elem(ID_EBML, &[0u8; 4]);
        file.extend_from_slice(&elem(ID_SEGMENT, &body));

        let mut cur = Cursor::new(&file);
        assert_eq!(
            attached_cover(&mut cur).as_deref(),
            Some(b"JPEGDATA".as_slice())
        );
    }

    #[test]
    fn attachments_behind_a_cluster_resolve_via_seekhead() {
        // Layout: SeekHead, Tracks, Cluster, Attachments — the front walk stops at the
        // Cluster, so only the SeekHead can reveal where the Attachments sit.
        let tracks = elem(
            ID_TRACKS,
            &elem(
                ID_TRACK_ENTRY,
                &[
                    elem(ID_TRACK_TYPE, &[TRACK_TYPE_VIDEO as u8]),
                    elem(ID_CODEC_ID, b"V_AV1"),
                ]
                .concat(),
            ),
        );
        let cluster = elem(ID_CLUSTER, &elem(ID_CLUSTER_TIMECODE, &[0]));
        let attachments = elem(
            ID_ATTACHMENTS,
            &elem(
                ID_ATTACHED_FILE,
                &[
                    elem(ID_FILE_NAME, b"poster.png"),
                    elem(ID_FILE_MIME, b"image/png"),
                    elem(ID_FILE_DATA, b"PNGDATA"),
                ]
                .concat(),
            ),
        );
        // SeekPosition is Segment-relative; a fixed 2-byte encoding keeps the SeekHead's own
        // length independent of the value, so one dummy pass sizes it and the second is real.
        let seekhead_for = |pos: u16| {
            elem(
                ID_SEEKHEAD,
                &elem(
                    ID_SEEK,
                    &[
                        elem(ID_SEEK_ID, &[0x19, 0x41, 0xA4, 0x69]),
                        elem(ID_SEEK_POSITION, &pos.to_be_bytes()),
                    ]
                    .concat(),
                ),
            )
        };
        let attach_pos = (seekhead_for(0).len() + tracks.len() + cluster.len()) as u16;
        let body = [seekhead_for(attach_pos), tracks, cluster, attachments].concat();
        let mut file = elem(ID_EBML, &[0u8; 4]);
        file.extend_from_slice(&elem(ID_SEGMENT, &body));

        let mut cur = Cursor::new(&file);
        assert_eq!(video_codec_id(&mut cur).as_deref(), Some("V_AV1"));
        assert_eq!(
            attached_cover(&mut cur).as_deref(),
            Some(b"PNGDATA".as_slice())
        );
    }

    /// End-to-end: parse a real MKV (path in `ST2K_TEST_MKV`) into a one-cluster mini-MKV and
    /// decode it through Media Foundation. Skipped when the env var isn't set / file is absent,
    /// so CI stays green without an adult-content fixture in the repo.
    #[test]
    fn real_mkv_round_trips_through_mediafoundation() {
        let Some(path) = std::env::var("ST2K_TEST_MKV")
            .ok()
            .filter(|p| Path::new(p).is_file())
        else {
            eprintln!("real_mkv_round_trips: ST2K_TEST_MKV unset / missing — skipping");
            return;
        };
        let bytes = std::fs::read(&path).expect("read sample mkv");
        let mini = keyframe_mini_mkv(&mut Cursor::new(&bytes), 0.30)
            .expect("build mini-mkv from real sample");
        assert!(
            mini[0..4] == [0x1A, 0x45, 0xDF, 0xA3],
            "starts with EBML header"
        );
        assert!(mini.len() < bytes.len(), "mini-mkv smaller than source");
        let frame = crate::video::frame_from_bytes(&mini)
            .expect("Media Foundation should decode the mini-mkv cluster");
        assert!(frame.width() > 0 && frame.height() > 0);
        eprintln!(
            "real_mkv_round_trips: mini {} bytes ({:.1} MB) → frame {}x{}",
            mini.len(),
            mini.len() as f64 / 1024.0 / 1024.0,
            frame.width(),
            frame.height()
        );
    }
}
