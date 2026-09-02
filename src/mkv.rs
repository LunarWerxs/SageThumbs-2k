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

// Byte-identical to mp4.rs's own helper; reuse it rather than keep a drifting private copy
// (flv.rs already does the same).
use crate::mp4::read_exact_at;

// EBML / Matroska element IDs (full IDs incl. the length-marker, as a big-endian integer).
// `pub(crate)`: `crate::fuzz`'s synthetic_mkv seed builds the same element tree these parsers
// walk, and used to keep its own drifting copy of this table (see `encode_vint`/`elem` below
// for the rest of that consolidation).
pub(crate) const ID_EBML: u64 = 0x1A45_DFA3;
pub(crate) const ID_SEGMENT: u64 = 0x1853_8067;
const ID_SEEKHEAD: u64 = 0x114D_9B74;
const ID_SEEK: u64 = 0x4DBB;
const ID_SEEK_ID: u64 = 0x53AB;
const ID_SEEK_POSITION: u64 = 0x53AC;
pub(crate) const ID_INFO: u64 = 0x1549_A966;
pub(crate) const ID_TIMECODE_SCALE: u64 = 0x2AD7B1;
pub(crate) const ID_DURATION: u64 = 0x4489;
pub(crate) const ID_TRACKS: u64 = 0x1654_AE6B;
pub(crate) const ID_TRACK_ENTRY: u64 = 0xAE;
pub(crate) const ID_TRACK_NUMBER: u64 = 0xD7;
pub(crate) const ID_TRACK_TYPE: u64 = 0x83;
/// `TrackEntry ▸ Video`, and the projection sub-tree inside it that carries rotation
/// (issue #32 — Matroska's answer to the MP4 display matrix).
const ID_VIDEO: u64 = 0xE0;
const ID_PROJECTION: u64 = 0x7670;
/// `ProjectionPoseRoll`. **0x7675, verified against a file ffmpeg actually wrote** — the first
/// draft of this used 0x7BBD, which is not a projection element at all, and the consequence
/// would have been the worst kind: an id that never matches makes this read silently return
/// "upright", which is indistinguishable from a video that IS upright. Nothing would have
/// failed; rotation would simply never have worked.
const ID_PROJECTION_POSE_ROLL: u64 = 0x7675;
pub(crate) const ID_CUES: u64 = 0x1C53_BB6B;
pub(crate) const ID_CUE_POINT: u64 = 0xBB;
pub(crate) const ID_CUE_TIME: u64 = 0xB3;
pub(crate) const ID_CUE_TRACK_POSITIONS: u64 = 0xB7;
pub(crate) const ID_CUE_TRACK: u64 = 0xF7;
pub(crate) const ID_CUE_CLUSTER_POSITION: u64 = 0xF1;
pub(crate) const ID_CLUSTER: u64 = 0x1F43_B675;
pub(crate) const ID_CLUSTER_TIMECODE: u64 = 0xE7;
const ID_SIMPLE_BLOCK: u64 = 0xA3;
const ID_BLOCK_GROUP: u64 = 0xA0;
const ID_BLOCK: u64 = 0xA1;
const ID_REFERENCE_BLOCK: u64 = 0xFB;
pub(crate) const ID_CODEC_ID: u64 = 0x86;
pub(crate) const ID_ATTACHMENTS: u64 = 0x1941_A469;
pub(crate) const ID_ATTACHED_FILE: u64 = 0x61A7;
pub(crate) const ID_FILE_NAME: u64 = 0x466E;
pub(crate) const ID_FILE_MIME: u64 = 0x4660;
pub(crate) const ID_FILE_DATA: u64 = 0x465C;

const TRACK_TYPE_VIDEO: u64 = 1;

// Sanity caps on the bounded elements we pull into memory.
const META_MAX: u64 = 8 * 1024 * 1024; // EBML header / Info / Tracks
const CUES_MAX: u64 = 32 * 1024 * 1024; // the index
const CLUSTER_MAX: u64 = 96 * 1024 * 1024; // one cluster (≤ a few seconds of 4K)
const ATTACH_MAX: u64 = 64 * 1024 * 1024; // all attachments (cover art + subtitle fonts)
/// Cap on CuePoint entries collected+sorted by [`cue_points`]. `CUES_MAX` already bounds the
/// body to 32 MiB, which indirectly bounds the entry count too (roughly 1-2M at typical
/// per-entry EBML overhead) — this makes that bound an explicit, independent one instead of
/// relying on how compact a hostile file's entries happen to be.
const CUE_POINTS_MAX: usize = 200_000;

/// Where a Matroska file's Segment metadata sits: the verbatim EBML header plus absolute
/// positions of the top-level children the readers below need — resolved by the
/// front-of-segment walk with the SeekHead filling in whatever sits past the first Cluster
/// (Cues and, sometimes, Attachments live at the file's end).
struct SegmentMap {
    ebml: Vec<u8>,
    /// Absolute file size (bounds checks) and Segment data start (Positions are relative to it).
    total: u64,
    seg_data: u64,
    /// Absolute end of the Segment (its declared end, or `total` for an unknown-size
    /// Segment). The bound an unknown-size Cluster resolves against — see
    /// [`unknown_cluster_end`].
    seg_end: u64,
    info: Option<u64>,
    tracks: Option<u64>,
    cues: Option<u64>,
    attachments: Option<u64>,
    /// Where the first Cluster begins — the front walk stops there anyway, so recording it
    /// is free, and it is the fallback for Cues-less files (`vp9_keyframe`): tiny WebMs
    /// (conformance vectors, screen grabs) routinely carry no index at all.
    first_cluster: Option<u64>,
}

/// What `scan_segment_front` collects: positions of the metadata children found before the
/// first Cluster, plus the raw SeekHead bytes (if any) for `resolve_via_seekhead` to consult.
struct FrontScan {
    seekhead: Option<Vec<u8>>,
    info: Option<u64>,
    tracks: Option<u64>,
    cues: Option<u64>,
    attachments: Option<u64>,
    cluster: Option<u64>,
}

/// Front-of-segment walk: capture SeekHead/Info/Tracks (and Cues/Attachments if they happen
/// to be up front), stopping at the first Cluster — we never scan the cluster body. `None`
/// only on the position-overflow edge case below, matching `segment_map`'s original `?`.
fn scan_segment_front<R: Read + Seek>(r: &mut R, seg_data: u64, seg_end: u64) -> Option<FrontScan> {
    let mut scan = FrontScan {
        seekhead: None,
        info: None,
        tracks: None,
        cues: None,
        attachments: None,
        cluster: None,
    };
    let mut p = seg_data;
    for _ in 0..64 {
        if p + 2 > seg_end {
            break;
        }
        // A header_at failure here (truncated read, reserved 0x00 marker byte, an
        // oversized VINT) used to propagate via `?` straight out of segment_map, discarding
        // every position already resolved (Info/Tracks/Cues/Attachments) even when the bad
        // element sits AFTER them. Treat it as end-of-walk instead: stop scanning forward,
        // but keep whatever this pass already found (the SeekHead resolution below still
        // runs against it).
        let Some((eid, esize, ehlen, eunk)) = header_at(r, p) else {
            break;
        };
        match eid {
            ID_SEEKHEAD if esize <= META_MAX => scan.seekhead = read_full_at(r, p + ehlen, esize),
            ID_INFO => scan.info = Some(p),
            ID_TRACKS => scan.tracks = Some(p),
            ID_CUES => scan.cues = Some(p),
            ID_ATTACHMENTS => scan.attachments = Some(p),
            ID_CLUSTER => {
                scan.cluster = Some(p);
                break;
            }
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
    Some(scan)
}

/// Resolve any position `scan_segment_front` didn't find via the SeekHead (Cues are
/// typically at the file's end). SeekPosition is an attacker-controlled EBML uint up to
/// u64::MAX; `checked_add` drops an entry that would overflow instead of wrapping to a bogus
/// offset (release, overflow-checks off) or panicking (debug/test) — matching the
/// checked_add already used for Cues-derived cluster positions elsewhere in this file.
fn resolve_via_seekhead(scan: &mut FrontScan, seg_data: u64) {
    let Some(sh) = &scan.seekhead else {
        return;
    };
    let cues = scan
        .cues
        .or_else(|| seek_lookup(sh, ID_CUES).and_then(|rel| seg_data.checked_add(rel)));
    let info = scan
        .info
        .or_else(|| seek_lookup(sh, ID_INFO).and_then(|rel| seg_data.checked_add(rel)));
    let tracks = scan
        .tracks
        .or_else(|| seek_lookup(sh, ID_TRACKS).and_then(|rel| seg_data.checked_add(rel)));
    let attachments = scan
        .attachments
        .or_else(|| seek_lookup(sh, ID_ATTACHMENTS).and_then(|rel| seg_data.checked_add(rel)));
    scan.cues = cues;
    scan.info = info;
    scan.tracks = tracks;
    scan.attachments = attachments;
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

    let mut scan = scan_segment_front(r, seg_data, seg_end)?;
    resolve_via_seekhead(&mut scan, seg_data);

    Some(SegmentMap {
        ebml,
        total,
        seg_data,
        seg_end,
        info: scan.info,
        tracks: scan.tracks,
        cues: scan.cues,
        attachments: scan.attachments,
        first_cluster: scan.cluster,
    })
}

/// Build a one-cluster mini-MKV for the keyframe nearest `fraction` of the running time, for
/// [`crate::video::frame_from_bytes`], plus the display rotation this same `Tracks` element
/// already carried — a caller that already has this need not re-read Tracks a
/// second time through [`display_rotation`] just to ask). `None` if the source isn't a
/// Cues-indexed Matroska/WebM (caller falls back to the bounded head-prefix tier).
pub fn keyframe_mini_mkv<R: Read + Seek>(
    r: &mut R,
    fraction: f64,
) -> Option<(Vec<u8>, Option<u32>)> {
    let map = segment_map(r)?;

    let (_, info_hlen, info) = read_element_full(r, map.info?, META_MAX, ID_INFO)?;
    let (_, tracks_hlen, tracks) = read_element_full(r, map.tracks?, META_MAX, ID_TRACKS)?;
    let (_, cues_hlen, cues) = read_element_full(r, map.cues?, CUES_MAX, ID_CUES)?;
    let tracks_body = &tracks[tracks_hlen..];
    let rotation = video_track_roll(tracks_body).and_then(rotation_from_roll);

    // Pick the cluster: video track number, the Cue list, then the cue nearest `fraction`.
    let video_track = video_track_number(tracks_body);
    let cluster_rel = cue_cluster_position(
        &cues[cues_hlen..],
        &info[info_hlen..],
        video_track,
        fraction,
    )?;
    let cluster_abs = map.seg_data.checked_add(cluster_rel)?;

    // Cues only promise the keyframe is SOMEWHERE in the cluster — verify it with
    // `cluster_keyframe` when the video track is known (mirroring `vp9_keyframe`'s own
    // candidate list below), falling back to the file's first Cluster when the cue-indexed
    // one turns out to hold no keyframe for that track.
    let mut candidates: Vec<u64> = Vec::new();
    if cluster_abs < map.total {
        candidates.push(cluster_abs);
    }
    if let Some(first) = map.first_cluster {
        if !candidates.contains(&first) {
            candidates.push(first);
        }
    }

    for candidate_abs in candidates {
        let Some((cluster_hlen, mut cluster)) = read_cluster(r, &map, candidate_abs) else {
            continue;
        };
        if let Some(vt) = video_track {
            if cluster_keyframe(&cluster[cluster_hlen..], vt).is_none() {
                continue; // no keyframe for the video track in this cluster — try the fallback
            }
        }
        // Zero the Cluster's Timecode so the mini-clip starts at t=0 (otherwise
        // `frame_from_bytes`'s near-the-head seek would land before the cluster's real
        // timestamp and grab nothing). Likewise zero Info's Duration so that seek computes ~0.
        zero_child(&mut cluster, cluster_hlen, ID_CLUSTER_TIMECODE);
        let mut info = info.clone();
        zero_child(&mut info, info_hlen, ID_DURATION);
        return Some((
            build_mini_mkv(&map.ebml, &info, &tracks, &cluster),
            rotation,
        ));
    }
    None
}

/// The Segment-relative position of the Cluster holding the keyframe nearest `fraction` of
/// the running time, from an in-memory Cues body. Shared by [`keyframe_mini_mkv`] (which
/// wraps that cluster for Media Foundation) and [`vp9_keyframe`] (which pulls the raw block
/// out of it), so the two can't disagree about WHICH frame represents the file.
fn cue_cluster_position(
    cues_body: &[u8],
    info_body: &[u8],
    video_track: Option<u64>,
    fraction: f64,
) -> Option<u64> {
    let (duration, _timescale) = info_duration(info_body);
    let cues_list = cue_points(cues_body, video_track);
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
    Some(cues_list[idx].1)
}

/// The raw bytes of one VP9 keyframe — the block payload as the encoder wrote it — for the
/// out-of-process `st2k vp9-frame` decoder (`crate::vp9`). Self-gates on the container
/// being Matroska/WebM whose FIRST VIDEO TRACK is `V_VP9`; everything else is `None`.
///
/// Cluster choice mirrors [`keyframe_mini_mkv`]: the Cues entry nearest `fraction` of the
/// running time when the file carries an index. Unlike that path, a Cues-less file falls
/// back to the FIRST cluster (tiny WebMs — conformance vectors, screen grabs — routinely
/// have no index at all, and their first block is the keyframe). Within the cluster, the
/// keyframe is the first video SimpleBlock with the key flag, or the first BlockGroup
/// without a ReferenceBlock; laced blocks are declined (see [`unlaced_frame`]).
pub fn vp9_keyframe<R: Read + Seek>(r: &mut R, fraction: f64) -> Option<Vec<u8>> {
    let map = segment_map(r)?;
    let (_, tracks_hlen, tracks) = read_element_full(r, map.tracks?, META_MAX, ID_TRACKS)?;
    let tracks_body = &tracks[tracks_hlen..];
    if video_track_codec(tracks_body).as_deref() != Some("V_VP9") {
        return None;
    }
    let video_track = video_track_number(tracks_body)?;

    // Preferred cluster from the Cues (representative mid-video frame), first cluster as
    // the fallback — also taken when the indexed cluster turns out to hold no keyframe
    // block we can use (e.g. its video blocks are laced).
    let mut candidates: Vec<u64> = Vec::new();
    if let (Some(cues_pos), Some(info_pos)) = (map.cues, map.info) {
        if let (Some((_, cues_hlen, cues)), Some((_, info_hlen, info))) = (
            read_element_full(r, cues_pos, CUES_MAX, ID_CUES),
            read_element_full(r, info_pos, META_MAX, ID_INFO),
        ) {
            if let Some(rel) = cue_cluster_position(
                &cues[cues_hlen..],
                &info[info_hlen..],
                Some(video_track),
                fraction,
            ) {
                if let Some(abs) = map.seg_data.checked_add(rel) {
                    candidates.push(abs);
                }
            }
        }
    }
    if let Some(first) = map.first_cluster {
        if !candidates.contains(&first) {
            candidates.push(first);
        }
    }

    for cluster_abs in candidates {
        if cluster_abs >= map.total {
            continue;
        }
        let Some((chlen, cluster)) = read_cluster(r, &map, cluster_abs) else {
            continue;
        };
        if let Some(frame) = cluster_keyframe(&cluster[chlen..], video_track) {
            return Some(frame);
        }
    }
    None
}

/// The first video-track KEYFRAME payload in a Cluster body: a SimpleBlock whose keyframe
/// flag (0x80) is set, or a BlockGroup whose Block carries no ReferenceBlock (that absence
/// IS Matroska's keyframe marker for grouped blocks).
fn cluster_keyframe(cluster_body: &[u8], video_track: u64) -> Option<Vec<u8>> {
    for (id, _, data) in children(cluster_body) {
        let frame = match id {
            ID_SIMPLE_BLOCK => simple_block_keyframe(data, video_track),
            ID_BLOCK_GROUP => block_group_keyframe(data, video_track),
            _ => None,
        };
        if frame.is_some() {
            return frame;
        }
    }
    None
}

/// A SimpleBlock's frame, when it belongs to `video_track` and its keyframe flag
/// (0x80) is set.
fn simple_block_keyframe(data: &[u8], video_track: u64) -> Option<Vec<u8>> {
    let (track, flags, frame) = parse_block(data)?;
    if track != video_track || flags & 0x80 == 0 {
        return None;
    }
    unlaced_frame(flags, frame).map(<[u8]>::to_vec)
}

/// A BlockGroup's Block frame, when it belongs to `video_track` and carries no
/// ReferenceBlock (that absence IS Matroska's keyframe marker for grouped blocks).
fn block_group_keyframe(data: &[u8], video_track: u64) -> Option<Vec<u8>> {
    let mut block = None;
    let mut has_ref = false;
    for (cid, _, cd) in children(data) {
        match cid {
            ID_BLOCK => block = Some(cd),
            ID_REFERENCE_BLOCK => has_ref = true,
            _ => {}
        }
    }
    if has_ref {
        return None;
    }
    let (track, flags, frame) = block.and_then(parse_block)?;
    if track != video_track {
        return None;
    }
    unlaced_frame(flags, frame).map(<[u8]>::to_vec)
}

/// Split a (Simple)Block body into `(track_number, flags, frame_bytes)`: a size-style vint
/// track number, a 2-byte relative timecode, one flags byte, then the frame data.
fn parse_block(data: &[u8]) -> Option<(u64, u8, &[u8])> {
    let (track, tlen, unknown) = vint_size(data, 0)?;
    if unknown {
        return None;
    }
    let flags = *data.get(tlen + 2)?;
    Some((track, flags, data.get(tlen + 3..)?))
}

/// The frame bytes of a block, only when it is UNLACED (lacing bits 0b110 clear). A laced
/// block packs several frames behind a lace-size table, and handing that table to a codec
/// as if it were bitstream would be garbage-in; video keyframes are never laced in
/// practice (lacing exists for tiny audio frames), so declining is a non-loss.
fn unlaced_frame(flags: u8, frame: &[u8]) -> Option<&[u8]> {
    if flags & 0b0000_0110 == 0 && !frame.is_empty() {
        Some(frame)
    } else {
        None
    }
}

/// The CodecID of the first video track ("V_MPEGH/ISO/HEVC", "V_AV1", …), for the doctor's
/// codec diagnosis. Cheap: reads the EBML head plus the Tracks element only — a few KB —
/// and `None` for non-Matroska sources or video-less files.
pub fn video_codec_id<R: Read + Seek>(r: &mut R) -> Option<String> {
    let map = segment_map(r)?;
    let (_, hlen, tracks) = read_element_full(r, map.tracks?, META_MAX, ID_TRACKS)?;
    video_track_codec(&tracks[hlen..])
}

/// The CLOCKWISE rotation, in degrees (90, 180 or 270), that this Matroska file's video track
/// asks a player to apply — the twin of [`crate::mp4::display_rotation`], and issue #32's
/// other half.
///
/// Matroska has no display matrix. It stores the same intent as `ProjectionPoseRoll`, a float
/// in DEGREES inside `TrackEntry ▸ Video ▸ Projection`, and FFmpeg converts between the two
/// forms — the same `ffmpeg -display_rotation 90 -c copy` into a `.mkv` produces a roll here
/// that `ffprobe` reports back as the identical display matrix it writes into an `.mp4`.
///
/// Cheap: the EBML head plus the Tracks element, a few KB, and `None` for non-Matroska
/// sources, video-less files and upright video.
pub fn display_rotation<R: Read + Seek>(r: &mut R) -> Option<u32> {
    let map = segment_map(r)?;
    let (_, hlen, tracks) = read_element_full(r, map.tracks?, META_MAX, ID_TRACKS)?;
    video_track_roll(tracks.get(hlen..)?).and_then(rotation_from_roll)
}

/// `ProjectionPoseRoll` of the first video TrackEntry, in degrees, or `None`.
fn video_track_roll(tracks_data: &[u8]) -> Option<f64> {
    let entry = video_track_entry(tracks_data)?;
    let (_, _, video) = children(entry).find(|(id, _, _)| *id == ID_VIDEO)?;
    let (_, _, proj) = children(video).find(|(id, _, _)| *id == ID_PROJECTION)?;
    let (_, _, roll) = children(proj).find(|(id, _, _)| *id == ID_PROJECTION_POSE_ROLL)?;
    ebml_float(roll)
}

/// Map a `ProjectionPoseRoll` to the clockwise angle to apply, or `None` for upright video
/// and for any roll that is not an exact quarter turn.
///
/// **The direction is measured, not assumed.** Remuxing the issue's own commands into
/// Matroska and reading the roll back out of the files gives:
///
/// ```text
///   ffmpeg -display_rotation …    roll written    ffprobe display matrix   -> clockwise
///                          90            +90.0    same as the .mp4's          270
///                         180           +180.0    same as the .mp4's          180
///                         270            -90.0    same as the .mp4's           90
/// ```
///
/// So the roll is COUNTER-clockwise degrees and is negated here. `mkv_roll_matches_the_mp4_matrix`
/// pins those rows against the MP4 mapper itself, because a silent disagreement between the two
/// containers would rotate one of them the wrong way while the other stayed right.
///
/// A float is compared with a tolerance rather than for equality: it is written by whichever
/// muxer produced the file, and 90.00000000000001 is a quarter turn.
fn rotation_from_roll(roll: f64) -> Option<u32> {
    if !roll.is_finite() {
        return None;
    }
    // Counter-clockwise degrees -> the clockwise angle we apply, normalised into [0, 360).
    let cw = (-roll).rem_euclid(360.0);
    [90u32, 180, 270]
        .into_iter()
        .find(|&candidate| (cw - f64::from(candidate)).abs() < 0.5)
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
///
/// Reads up to 12 bytes (4-byte ID max + 8-byte size max, the widest an EBML header can be)
/// in ONE bulk read instead of up to 12 separate single-byte ones. `IStreamReader` (the
/// shell-COM backing reader used when this walks a live Explorer stream) has no internal
/// buffering, so each single-byte read used to cost its own marshaled COM round trip — up to
/// 12 of them per header, and `segment_map`'s walk can call this dozens of times per file.
fn header_at<R: Read + Seek>(r: &mut R, pos: u64) -> Option<(u64, u64, u64, bool)> {
    r.seek(SeekFrom::Start(pos)).ok()?;
    let mut buf = [0u8; 12];
    let mut have = 0usize;
    while have < buf.len() {
        match r.read(&mut buf[have..]) {
            // A short read here just means the header we actually need (which may be far
            // fewer than 12 bytes) fits before EOF; validated below by `have`, not here.
            Ok(0) => break,
            Ok(n) => have += n,
            Err(_) => return None,
        }
    }
    let buf = &buf[..have];

    // Element ID: 1–4 bytes, value keeps the length-marker bit.
    let b0 = *buf.first()?;
    if b0 == 0 {
        return None;
    }
    let id_len = b0.leading_zeros() as usize + 1;
    if id_len > 4 || id_len > have {
        return None;
    }
    let mut id = 0u64;
    for &b in &buf[..id_len] {
        id = (id << 8) | b as u64;
    }

    // Size: 1–8 bytes, value strips the marker bit; all-ones data = unknown size.
    let sb0 = *buf.get(id_len)?;
    if sb0 == 0 {
        return None;
    }
    let sz_len = sb0.leading_zeros() as usize + 1;
    if sz_len > 8 || id_len + sz_len > have {
        return None;
    }
    // Widen before shifting: an 8-byte size vint (first byte 0x01 — ffmpeg writes the
    // Segment size this way routinely) needs `0xFF >> 8`, which overflows a u8 shift.
    // The u8 version panicked in debug and, worse, silently produced mask 0xFF in release —
    // a phantom 2^56 in every 8-byte size and unknown-size never detected.
    let mask = (0xFFu16 >> sz_len) as u8;
    let size_bytes = &buf[id_len..id_len + sz_len];
    let mut size = (size_bytes[0] & mask) as u64;
    let mut all_ones = (size_bytes[0] & mask) == mask;
    for &b in &size_bytes[1..] {
        size = (size << 8) | b as u64;
        if b != 0xFF {
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

/// The real end of a Cluster at `pos` whose EBML size marker is "unknown" (all-ones) —
/// real, never-finalized files write this for the last Cluster (and some streaming muxers
/// write it for every Cluster). A Cluster is a top-level Segment child, so its end is either
/// the position of the next top-level element the front-of-segment walk already resolved
/// (only meaningful when `pos` is that walk's own [`SegmentMap::first_cluster`], since later
/// clusters were never individually located) or the Segment's own end, whichever comes
/// first — capped at `pos + CLUSTER_MAX` like every other cluster read in this module.
fn unknown_cluster_end(map: &SegmentMap, pos: u64) -> u64 {
    let next_known = [map.info, map.tracks, map.cues, map.attachments]
        .into_iter()
        .flatten()
        .filter(|&p| p > pos)
        .min();
    let end = next_known
        .unwrap_or(map.seg_end)
        .min(map.seg_end)
        .min(map.total);
    end.min(pos.saturating_add(CLUSTER_MAX))
}

/// Read the Cluster at `pos`, resolving an EBML "unknown size" marker to a real byte range
/// (see [`unknown_cluster_end`]) instead of declining outright the way [`read_element_full`]
/// does. Returns `(header_len, full_element_bytes)`.
fn read_cluster<R: Read + Seek>(r: &mut R, map: &SegmentMap, pos: u64) -> Option<(usize, Vec<u8>)> {
    let (id, size, hlen, unknown) = header_at(r, pos)?;
    if id != ID_CLUSTER {
        return None;
    }
    let total = if unknown {
        unknown_cluster_end(map, pos).checked_sub(pos)?
    } else {
        hlen.checked_add(size)?
    };
    if total < hlen || total > CLUSTER_MAX {
        return None;
    }
    let mut buf = vec![0u8; total as usize];
    read_exact_at(r, pos, &mut buf)?;
    Some((hlen as usize, buf))
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

/// The first video (`TrackType == 1`) TrackEntry's body within a `Tracks` element, or `None`
/// when the file has no video track. Shared by [`video_track_number`], [`video_track_codec`]
/// and [`video_track_roll`], which each used to repeat this same "find TrackEntry, check
/// TrackType" walk independently.
fn video_track_entry(tracks_data: &[u8]) -> Option<&[u8]> {
    for (id, _, entry) in children(tracks_data) {
        if id != ID_TRACK_ENTRY {
            continue;
        }
        let ttype =
            children(entry).find_map(|(cid, _, cd)| (cid == ID_TRACK_TYPE).then(|| ebml_uint(cd)));
        if ttype == Some(TRACK_TYPE_VIDEO) {
            return Some(entry);
        }
    }
    None
}

/// TrackNumber of the first video TrackEntry (TrackType == 1), or `None`.
fn video_track_number(tracks_data: &[u8]) -> Option<u64> {
    let entry = video_track_entry(tracks_data)?;
    children(entry).find_map(|(cid, _, cd)| (cid == ID_TRACK_NUMBER).then(|| ebml_uint(cd)))
}

/// CodecID string of the first video TrackEntry (TrackType == 1), or `None`.
fn video_track_codec(tracks_data: &[u8]) -> Option<String> {
    let entry = video_track_entry(tracks_data)?;
    children(entry).find_map(|(cid, _, cd)| {
        (cid == ID_CODEC_ID).then(|| {
            String::from_utf8_lossy(cd)
                .trim_end_matches('\0')
                .to_string()
        })
    })
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

/// One CueTrackPositions child: `(this track's position if it's the video track, this track's
/// cluster position as the any-track fallback)`.
fn cue_track_position(cd: &[u8], video_track: Option<u64>) -> (Option<u64>, Option<u64>) {
    let mut track = None;
    let mut cpos = None;
    for (tid, _, td) in children(cd) {
        match tid {
            ID_CUE_TRACK => track = Some(ebml_uint(td)),
            ID_CUE_CLUSTER_POSITION => cpos = Some(ebml_uint(td)),
            _ => {}
        }
    }
    let Some(cpos) = cpos else {
        return (None, None);
    };
    let video_pos = (video_track.is_none() || track == video_track).then_some(cpos);
    (video_pos, Some(cpos))
}

/// One CuePoint's `(cue_time, cluster_segment_position)`, preferring the video track's
/// CueTrackPositions (falling back to the first track's, across possibly several occurrences).
fn parse_cue_point(cp: &[u8], video_track: Option<u64>) -> Option<(u64, u64)> {
    let mut time = None;
    let mut pos = None; // video-track position
    let mut first_pos = None; // any-track fallback
    for (cid, _, cd) in children(cp) {
        match cid {
            ID_CUE_TIME => time = Some(ebml_uint(cd)),
            ID_CUE_TRACK_POSITIONS => {
                let (video_pos, any_pos) = cue_track_position(cd, video_track);
                if let Some(any_pos) = any_pos {
                    first_pos.get_or_insert(any_pos);
                }
                if pos.is_none() {
                    pos = video_pos;
                }
            }
            _ => {}
        }
    }
    Some((time?, pos.or(first_pos)?))
}

/// `(cue_time, cluster_segment_position)` for each CuePoint, preferring the video track's
/// CueTrackPositions (falling back to the first). Sorted ascending by time.
fn cue_points(cues_data: &[u8], video_track: Option<u64>) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for (id, _, cp) in children(cues_data) {
        if out.len() >= CUE_POINTS_MAX {
            break;
        }
        if id != ID_CUE_POINT {
            continue;
        }
        if let Some(entry) = parse_cue_point(cp, video_track) {
            out.push(entry);
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
pub(crate) fn encode_vint(n: u64) -> Vec<u8> {
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

/// Emit one EBML element: id bytes (as stored in the `ID_*` constants) + size vint + data.
/// Test/fuzz-only (moved out of `mod tests` to module scope so `crate::fuzz`'s synthetic_mkv
/// seed can reuse it, rather than maintaining a parallel encoder that could silently desync
/// from the real ID table / vint format).
#[cfg(test)]
pub(crate) fn elem(id: u64, data: &[u8]) -> Vec<u8> {
    let id_bytes = id.to_be_bytes();
    let start = id_bytes.iter().position(|&b| b != 0).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&id_bytes[start..]);
    out.extend_from_slice(&encode_vint(data.len() as u64));
    out.extend_from_slice(data);
    out
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

    // --- vp9_keyframe: the raw-block extraction for the out-of-process VP9 decoder -------

    /// A minimal VP9 Matroska: Tracks (track 1 = V_VP9 video) + one Cluster of blocks.
    fn vp9_mkv(cluster_children: &[Vec<u8>]) -> Vec<u8> {
        let tracks = elem(
            ID_TRACKS,
            &elem(
                ID_TRACK_ENTRY,
                &[
                    elem(ID_TRACK_NUMBER, &[1]),
                    elem(ID_TRACK_TYPE, &[TRACK_TYPE_VIDEO as u8]),
                    elem(ID_CODEC_ID, b"V_VP9"),
                ]
                .concat(),
            ),
        );
        let cluster = elem(
            ID_CLUSTER,
            &[
                elem(ID_CLUSTER_TIMECODE, &[0]).as_slice(),
                &cluster_children.concat(),
            ]
            .concat(),
        );
        let mut file = elem(ID_EBML, &[0u8; 4]);
        file.extend_from_slice(&elem(ID_SEGMENT, &[tracks, cluster].concat()));
        file
    }

    /// A SimpleBlock for track 1: flags byte as given, then the frame bytes.
    fn simple_block(flags: u8, frame: &[u8]) -> Vec<u8> {
        let mut body = vec![0x81, 0, 0, flags]; // track vint (1), timecode, flags
        body.extend_from_slice(frame);
        elem(ID_SIMPLE_BLOCK, &body)
    }

    #[test]
    fn vp9_keyframe_finds_the_first_keyframe_simpleblock() {
        // An inter block first (no key flag) — must be skipped; then the keyframe.
        let file = vp9_mkv(&[
            simple_block(0x00, &[0xEE; 8]),
            simple_block(0x80, &[0x86, 0x00, 0x42, 0x11, 0x22]),
        ]);
        assert_eq!(
            vp9_keyframe(&mut Cursor::new(&file), 0.30).as_deref(),
            Some([0x86, 0x00, 0x42, 0x11, 0x22].as_slice())
        );
    }

    #[test]
    fn vp9_keyframe_reads_blockgroups_and_lacing_rules() {
        // A BlockGroup WITH a ReferenceBlock is an inter frame; one WITHOUT is the key.
        let inter_group = elem(
            ID_BLOCK_GROUP,
            &[
                elem(ID_BLOCK, &[0x81, 0, 0, 0x00, 0xAA, 0xBB]),
                elem(ID_REFERENCE_BLOCK, &[0x7F]),
            ]
            .concat(),
        );
        let key_group = elem(
            ID_BLOCK_GROUP,
            &elem(ID_BLOCK, &[0x81, 0, 0, 0x00, 0xCC, 0xDD]),
        );
        let file = vp9_mkv(&[inter_group, key_group]);
        assert_eq!(
            vp9_keyframe(&mut Cursor::new(&file), 0.30).as_deref(),
            Some([0xCC, 0xDD].as_slice())
        );
        // A LACED keyframe block (lacing bits set) is declined, not mis-sliced.
        let laced = vp9_mkv(&[simple_block(0x80 | 0x06, &[2, 0x11, 0x22, 0x33, 0x44])]);
        assert_eq!(vp9_keyframe(&mut Cursor::new(&laced), 0.30), None);
    }

    #[test]
    fn vp9_keyframe_gates_on_the_codec_and_survives_junk() {
        // Same structure, wrong codec: the extraction must decline — this gate is what
        // keeps every non-VP9 video from paying for a child-process attempt.
        let mut vp8 = vp9_mkv(&[simple_block(0x80, &[0x11; 6])]);
        let at = vp8
            .windows(5)
            .position(|w| w == b"V_VP9")
            .expect("codec id present");
        vp8[at + 4] = b'8';
        assert_eq!(vp9_keyframe(&mut Cursor::new(&vp8), 0.30), None);
        // Junk + every truncation: Err/None only, never a panic.
        assert_eq!(vp9_keyframe(&mut Cursor::new(&b"junk"[..]), 0.30), None);
        let whole = vp9_mkv(&[simple_block(0x80, &[0x55; 16])]);
        for n in 0..whole.len() {
            let _ = vp9_keyframe(&mut Cursor::new(&whole[..n]), 0.30);
        }
    }

    /// The real FATE Profile 2 vector must yield a keyframe payload (it has no Cues, so
    /// this also covers the first-cluster fallback). Skips when the corpus is absent (CI).
    #[test]
    fn corpus_vp9_profile2_yields_a_keyframe() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("test-corpus")
            .join("sample-vp9p2.webm");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("corpus_vp9_profile2: no sample-vp9p2.webm — skipping");
            return;
        };
        let frame = vp9_keyframe(&mut Cursor::new(&bytes), 0.30)
            .expect("FATE vp9 profile-2 vector should yield a keyframe block");
        assert!(!frame.is_empty());
        // VP9 frame marker: top two bits of the first byte are 0b10.
        assert_eq!(frame[0] >> 6, 0b10, "payload should start a VP9 frame");
    }

    /// A Cluster whose EBML size is "unknown" (the all-ones marker) — real,
    /// never-finalized encoder output for the LAST Cluster in a file — must be resolved to
    /// its real extent (here, the Segment's own end, since nothing follows it) instead of
    /// being declined outright.
    #[test]
    fn vp9_keyframe_resolves_an_unknown_size_last_cluster() {
        let tracks = elem(
            ID_TRACKS,
            &elem(
                ID_TRACK_ENTRY,
                &[
                    elem(ID_TRACK_NUMBER, &[1]),
                    elem(ID_TRACK_TYPE, &[TRACK_TYPE_VIDEO as u8]),
                    elem(ID_CODEC_ID, b"V_VP9"),
                ]
                .concat(),
            ),
        );
        let cluster_body = [
            elem(ID_CLUSTER_TIMECODE, &[0]),
            simple_block(0x80, &[0x86, 0x00, 0x42, 0x11, 0x22]),
        ]
        .concat();
        // Hand-built unknown-size Cluster header: the 4-byte Cluster ID + a 1-byte
        // all-ones size vint (0xFF), which `elem` (definite-size only) cannot produce.
        let mut cluster = vec![0x1F, 0x43, 0xB6, 0x75, 0xFF];
        cluster.extend_from_slice(&cluster_body);

        let mut file = elem(ID_EBML, &[0u8; 4]);
        file.extend_from_slice(&elem(ID_SEGMENT, &[tracks, cluster].concat()));

        let frame = vp9_keyframe(&mut Cursor::new(&file), 0.30)
            .expect("an unknown-size last Cluster must still be resolved and read");
        assert_eq!(frame, [0x86, 0x00, 0x42, 0x11, 0x22]);
    }

    /// The Cue-indexed cluster only PROMISES a keyframe is somewhere inside it —
    /// `keyframe_mini_mkv` must verify with `cluster_keyframe` and fall back to the file's
    /// first Cluster (mirroring `vp9_keyframe`'s own candidate list) when the cued one turns
    /// out to hold none for the video track.
    #[test]
    fn keyframe_mini_mkv_falls_back_when_the_cued_cluster_has_no_keyframe() {
        let (file, _bad_rel) = mini_mkv_two_clusters(true);
        let (mini, _rotation) = keyframe_mini_mkv(&mut Cursor::new(&file), 0.30)
            .expect("must fall back to the good first cluster");
        assert!(
            mini.windows(4).any(|w| w == [0x86, 0x00, 0x11, 0x22]),
            "the GOOD cluster's keyframe payload must be in the mini-mkv"
        );
        assert!(
            !mini.windows(4).any(|w| w == [0xEE, 0xEE, 0xEE, 0xEE]),
            "the BAD (keyframe-less) cluster must not have been used"
        );
    }

    /// The decline half of the same fix: when NEITHER the cued cluster NOR the file's first
    /// cluster holds a keyframe for the video track, `keyframe_mini_mkv` must give up rather
    /// than build a mini-clip around a cluster it never verified.
    #[test]
    fn keyframe_mini_mkv_declines_when_no_candidate_cluster_has_a_keyframe() {
        let (file, _bad_rel) = mini_mkv_two_clusters(false);
        assert_eq!(keyframe_mini_mkv(&mut Cursor::new(&file), 0.30), None);
    }

    /// Build a Segment: SeekHead (pointing at Cues), Info, Tracks, a "good" first Cluster
    /// (a real video keyframe), a "bad" second Cluster (no keyframe — an inter block), then
    /// Cues whose one entry points at the BAD cluster. `good_cluster_has_keyframe` swaps the
    /// good cluster's block for another keyframe-less one, for the decline-path test.
    /// Returns `(file_bytes, bad_cluster_segment_relative_position)`.
    fn mini_mkv_two_clusters(good_cluster_has_keyframe: bool) -> (Vec<u8>, u32) {
        let tracks = elem(
            ID_TRACKS,
            &elem(
                ID_TRACK_ENTRY,
                &[
                    elem(ID_TRACK_NUMBER, &[1]),
                    elem(ID_TRACK_TYPE, &[TRACK_TYPE_VIDEO as u8]),
                    elem(ID_CODEC_ID, b"V_VP9"),
                ]
                .concat(),
            ),
        );
        let info = elem(ID_INFO, &[]);
        let good_block = if good_cluster_has_keyframe {
            simple_block(0x80, &[0x86, 0x00, 0x11, 0x22])
        } else {
            simple_block(0x00, &[0xEE; 4])
        };
        let cluster_good = elem(
            ID_CLUSTER,
            &[elem(ID_CLUSTER_TIMECODE, &[0]), good_block].concat(),
        );
        let cluster_bad = elem(
            ID_CLUSTER,
            &[
                elem(ID_CLUSTER_TIMECODE, &[0]),
                simple_block(0x00, &[0xEE; 4]),
            ]
            .concat(),
        );
        let seekhead_for = |pos: u16| {
            elem(
                ID_SEEKHEAD,
                &elem(
                    ID_SEEK,
                    &[
                        elem(ID_SEEK_ID, &[0x1C, 0x53, 0xBB, 0x6B]), // Cues
                        elem(ID_SEEK_POSITION, &pos.to_be_bytes()),
                    ]
                    .concat(),
                ),
            )
        };
        let bad_rel =
            (seekhead_for(0).len() + info.len() + tracks.len() + cluster_good.len()) as u32;
        let cues_rel = bad_rel + cluster_bad.len() as u32;
        let cues = elem(
            ID_CUES,
            &elem(
                ID_CUE_POINT,
                &[
                    elem(ID_CUE_TIME, &[0]),
                    elem(
                        ID_CUE_TRACK_POSITIONS,
                        &[
                            elem(ID_CUE_TRACK, &[1]),
                            elem(ID_CUE_CLUSTER_POSITION, &bad_rel.to_be_bytes()),
                        ]
                        .concat(),
                    ),
                ]
                .concat(),
            ),
        );
        let body = [
            seekhead_for(cues_rel as u16),
            info,
            tracks,
            cluster_good,
            cluster_bad,
            cues,
        ]
        .concat();
        let mut file = elem(ID_EBML, &[0u8; 4]);
        file.extend_from_slice(&elem(ID_SEGMENT, &body));
        (file, bad_rel)
    }

    /// `keyframe_mini_mkv` must hand back the rotation it already read off the
    /// same `Tracks` element it walked for the video track/keyframe, instead of making the
    /// caller re-scan Tracks a second time through `display_rotation`.
    #[test]
    fn keyframe_mini_mkv_returns_the_rotation_it_already_parsed() {
        let tracks = elem(
            ID_TRACKS,
            &elem(
                ID_TRACK_ENTRY,
                &[
                    elem(ID_TRACK_NUMBER, &[1]),
                    elem(ID_TRACK_TYPE, &[TRACK_TYPE_VIDEO as u8]),
                    elem(ID_CODEC_ID, b"V_VP9"),
                    elem(
                        ID_VIDEO,
                        &elem(
                            ID_PROJECTION,
                            &elem(ID_PROJECTION_POSE_ROLL, &90.0f64.to_be_bytes()),
                        ),
                    ),
                ]
                .concat(),
            ),
        );
        let info = elem(ID_INFO, &[]);
        let cluster = elem(
            ID_CLUSTER,
            &[
                elem(ID_CLUSTER_TIMECODE, &[0]),
                simple_block(0x80, &[0x86, 0x00, 0x11, 0x22]),
            ]
            .concat(),
        );
        let seekhead_for = |pos: u16| {
            elem(
                ID_SEEKHEAD,
                &elem(
                    ID_SEEK,
                    &[
                        elem(ID_SEEK_ID, &[0x1C, 0x53, 0xBB, 0x6B]), // Cues
                        elem(ID_SEEK_POSITION, &pos.to_be_bytes()),
                    ]
                    .concat(),
                ),
            )
        };
        let cluster_rel = (seekhead_for(0).len() + info.len() + tracks.len()) as u32;
        let cues_rel = cluster_rel + cluster.len() as u32;
        let cues = elem(
            ID_CUES,
            &elem(
                ID_CUE_POINT,
                &[
                    elem(ID_CUE_TIME, &[0]),
                    elem(
                        ID_CUE_TRACK_POSITIONS,
                        &[
                            elem(ID_CUE_TRACK, &[1]),
                            elem(ID_CUE_CLUSTER_POSITION, &cluster_rel.to_be_bytes()),
                        ]
                        .concat(),
                    ),
                ]
                .concat(),
            ),
        );
        let body = [seekhead_for(cues_rel as u16), info, tracks, cluster, cues].concat();
        let mut file = elem(ID_EBML, &[0u8; 4]);
        file.extend_from_slice(&elem(ID_SEGMENT, &body));

        let (_mini, rotation) = keyframe_mini_mkv(&mut Cursor::new(&file), 0.30)
            .expect("synthetic Cues-indexed mkv should yield a mini-mkv");
        // ProjectionPoseRoll of +90 (counter-clockwise) maps to 270 clockwise — the same
        // measured mapping `rotation_from_roll`'s own tests pin.
        assert_eq!(rotation, Some(270));
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
        let (mini, _rotation) = keyframe_mini_mkv(&mut Cursor::new(&bytes), 0.30)
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

    // --- segment_map robustness -----------------------------------------------------------

    /// A malformed element (reserved 0x00 ID/size marker byte) sitting after Tracks in the
    /// front-of-segment walk used to make `header_at(r, p)?` propagate `None` straight out of
    /// `segment_map`, discarding the Tracks position already found. It must instead stop the
    /// walk there and keep what was already resolved.
    #[test]
    fn malformed_element_after_tracks_does_not_abort_the_whole_walk() {
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
        let mut body = tracks;
        // Two bytes so the loop's `p + 2 > seg_end` pre-check doesn't just break on its own
        // before header_at ever runs — this must exercise header_at returning None, not the
        // ordinary "ran out of room" exit.
        body.push(0x00);
        body.push(0x00);
        let mut file = elem(ID_EBML, &[0u8; 4]);
        file.extend_from_slice(&elem(ID_SEGMENT, &body));

        let mut cur = Cursor::new(&file);
        assert_eq!(
            video_codec_id(&mut cur).as_deref(),
            Some("V_AV1"),
            "a malformed element after Tracks must not erase the Tracks already found"
        );
    }

    /// A SeekHead SeekPosition large enough that `seg_data + rel` overflows u64 must be
    /// dropped (via `checked_add`), not wrapped (release) or panicked on (debug/test, where
    /// overflow-checks are on by default) — and it must not poison resolution of the OTHER
    /// front-of-segment data the same walk already found directly.
    #[test]
    fn seekhead_position_overflow_is_dropped_not_wrapped() {
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
        let huge_pos = u64::MAX - 1;
        let seekhead = elem(
            ID_SEEKHEAD,
            &elem(
                ID_SEEK,
                &[
                    elem(ID_SEEK_ID, &[0x19, 0x41, 0xA4, 0x69]), // Attachments
                    elem(ID_SEEK_POSITION, &huge_pos.to_be_bytes()),
                ]
                .concat(),
            ),
        );
        let body = [seekhead, tracks, cluster].concat();
        let mut file = elem(ID_EBML, &[0u8; 4]);
        file.extend_from_slice(&elem(ID_SEGMENT, &body));

        let mut cur = Cursor::new(&file);
        // Must not panic, and must not resolve Attachments to a bogus wrapped offset.
        assert_eq!(attached_cover(&mut cur), None);
        // The overflowing entry must not poison the rest of the walk.
        assert_eq!(video_codec_id(&mut cur).as_deref(), Some("V_AV1"));
    }

    // --- header_at bulk read ----------------------------------------------------------------

    /// The bulk-read rewrite must parse identically to the old byte-at-a-time version even
    /// when fewer than the full 12-byte scratch buffer are actually available (a header near
    /// EOF) — the short read must not be mistaken for "header truncated" when the header
    /// itself needed fewer bytes than that.
    #[test]
    fn header_at_parses_a_short_header_right_at_eof() {
        // A 2-byte header (1-byte ID + 1-byte size) with nothing after it at all: the fixed
        // 12-byte scratch buffer only gets 2 bytes back, which must not be mistaken for a
        // truncated read of a header that only ever needed 2.
        let file = elem(ID_CUE_POINT, &[]);
        let mut cur = Cursor::new(&file);
        let (id, size, hlen, unknown) = header_at(&mut cur, 0).expect("short header at EOF");
        assert_eq!(id, ID_CUE_POINT);
        assert_eq!(size, 0);
        assert_eq!(hlen, 2);
        assert!(!unknown);
    }

    /// A header whose ID/size vints genuinely run past EOF must still decline, not read
    /// garbage from the fixed-size scratch buffer.
    #[test]
    fn header_at_declines_a_genuinely_truncated_header() {
        // A 4-byte ID marker (0x10..) with only 2 bytes total available — needs 4+ but has 2.
        let truncated = [0x10u8, 0x00];
        let mut cur = Cursor::new(&truncated[..]);
        assert_eq!(header_at(&mut cur, 0), None);
    }

    // --- cue_points cap ----------------------------------------------------------------------

    /// `cue_points` must never collect more than `CUE_POINTS_MAX` entries, independent of how
    /// small each entry manages to be within the 32 MiB `CUES_MAX` body cap.
    #[test]
    fn cue_points_collection_is_capped() {
        let one = |t: u16| {
            let ctp = [
                elem(ID_CUE_TRACK, &[1]),
                elem(ID_CUE_CLUSTER_POSITION, &(t as u32).to_be_bytes()),
            ]
            .concat();
            elem(
                ID_CUE_POINT,
                &[
                    elem(ID_CUE_TIME, &t.to_be_bytes()),
                    elem(ID_CUE_TRACK_POSITIONS, &ctp),
                ]
                .concat(),
            )
        };
        let mut cues = Vec::new();
        for t in 0..(CUE_POINTS_MAX + 10) as u32 {
            cues.extend_from_slice(&one(t as u16));
        }
        let list = cue_points(&cues, Some(1));
        assert_eq!(list.len(), CUE_POINTS_MAX);
    }

    /// Issue #32, the Matroska half. Matroska has no display matrix; it stores the same
    /// intent as a `ProjectionPoseRoll` float in degrees, and the sign is NOT the same as
    /// the MP4 matrix, so the two halves have to be pinned against each other or one
    /// container silently rotates the wrong way.
    ///
    /// The numbers are measured, by reading the roll back out of files ffmpeg wrote:
    /// `-display_rotation 90` into a `.mkv` writes a roll of +90, and ffprobe reports that
    /// file as carrying the identical display matrix as the `.mp4` - which the MP4 side
    /// maps to 270 clockwise. So a roll of +90 must produce 270 here, and this asserts it
    /// BY CALLING THE MP4 MAPPER rather than by restating its answer.
    #[test]
    fn mkv_roll_matches_the_mp4_matrix() {
        const ONE: i32 = 1 << 16;
        // (roll degrees written by ffmpeg, the equivalent MP4 matrix a, b, c, d)
        let pairs = [
            (90.0, (0, -ONE, ONE, 0)),
            (180.0, (-ONE, 0, 0, -ONE)),
            (-90.0, (0, ONE, -ONE, 0)),
        ];
        for (roll, (a, b, c, d)) in pairs {
            let via_mkv = rotation_from_roll(roll);
            let via_mp4 = crate::mp4::rotation_from_matrix_for_tests(a, b, c, d);
            assert_eq!(
                via_mkv, via_mp4,
                "a roll of {roll} and its equivalent display matrix must agree"
            );
            assert!(via_mkv.is_some(), "a roll of {roll} must rotate something");
        }
    }

    /// Upright video, non-quarter turns, and the values a float can actually arrive as.
    #[test]
    fn only_quarter_turns_rotate_anything() {
        assert_eq!(rotation_from_roll(0.0), None, "upright");
        assert_eq!(
            rotation_from_roll(-0.0),
            None,
            "negative zero is still upright"
        );
        assert_eq!(rotation_from_roll(360.0), None, "a full turn is upright");
        assert_eq!(rotation_from_roll(45.0), None, "not a quarter turn");
        assert_eq!(rotation_from_roll(f64::NAN), None, "NaN");
        assert_eq!(rotation_from_roll(f64::INFINITY), None, "infinity");

        // Written by a muxer, not by hand: a hair off a quarter turn is a quarter turn.
        assert_eq!(rotation_from_roll(90.000000001), Some(270));
        assert_eq!(rotation_from_roll(-89.9999), Some(90));
        // And the wrap-around forms mean the same thing as their in-range twins.
        assert_eq!(rotation_from_roll(270.0), rotation_from_roll(-90.0));
        assert_eq!(rotation_from_roll(-180.0), rotation_from_roll(180.0));
    }

    /// The descent itself: the roll must come from the VIDEO track's Projection, not from a
    /// sibling track and not from a Video element that has no Projection at all.
    #[test]
    fn the_roll_is_read_from_the_video_tracks_projection() {
        // A subtitle track (type 17) claiming a roll, then the real video track.
        let decoy = track_entry(17, Some(180.0));
        let video = track_entry(1, Some(90.0));
        let mut tracks = decoy.clone();
        tracks.extend_from_slice(&video);
        assert_eq!(video_track_roll(&tracks), Some(90.0));

        // A video track with no Projection is upright, not an error.
        assert_eq!(video_track_roll(&track_entry(1, None)), None);
        // No video track at all.
        assert_eq!(video_track_roll(&decoy), None);
        // Garbage is a clean miss, never a panic (panic = "abort" in the shell host).
        assert_eq!(video_track_roll(&[0xFF; 32]), None);
        assert_eq!(video_track_roll(&[]), None);
    }

    /// A TrackEntry of `track_type`, optionally carrying Video > Projection > PoseRoll.
    fn track_entry(track_type: u8, roll: Option<f64>) -> Vec<u8> {
        let mut entry = vec![0x83, 0x81, track_type]; // TrackType
        if let Some(roll) = roll {
            let mut pose = vec![0x76, 0x75, 0x88]; // ProjectionPoseRoll, 8-byte float
            pose.extend_from_slice(&roll.to_be_bytes());
            let mut proj = vec![0x76, 0x70, 0x80 | pose.len() as u8]; // Projection
            proj.extend_from_slice(&pose);
            let mut video = vec![0xE0, 0x80 | proj.len() as u8]; // Video
            video.extend_from_slice(&proj);
            entry.extend_from_slice(&video);
        }
        let mut out = vec![0xAE, 0x80 | entry.len() as u8]; // TrackEntry
        out.extend_from_slice(&entry);
        out
    }
}
