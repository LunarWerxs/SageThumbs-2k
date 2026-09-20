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
mod ids;
use ids::*;
mod ebml;
use ebml::*;
mod cues;
use cues::*;
#[cfg(test)]
pub(crate) use ebml::elem;
pub(crate) use ebml::encode_vint;
pub(crate) use ids::{
    ID_ATTACHED_FILE, ID_ATTACHMENTS, ID_CLUSTER, ID_CLUSTER_TIMECODE, ID_CODEC_ID,
    ID_CODEC_PRIVATE, ID_CUES, ID_CUE_CLUSTER_POSITION, ID_CUE_POINT, ID_CUE_TIME, ID_CUE_TRACK,
    ID_CUE_TRACK_POSITIONS, ID_DURATION, ID_EBML, ID_FILE_DATA, ID_FILE_MIME, ID_FILE_NAME,
    ID_INFO, ID_SEGMENT, ID_TIMECODE_SCALE, ID_TRACKS, ID_TRACK_ENTRY, ID_TRACK_NUMBER,
    ID_TRACK_TYPE,
};

// Byte-identical to mp4.rs's own helper; reuse it rather than keep a drifting private copy
// (flv.rs already does the same).
use crate::mp4::read_exact_at;

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

    mini_mkv_from_candidates(
        r,
        &map,
        &candidates,
        video_track,
        &info,
        info_hlen,
        &tracks,
        rotation,
    )
}

/// Try each candidate Cluster in order: the first one that holds a keyframe for the video
/// track (when known) has its Timecode and Info's Duration zeroed and is muxed into the
/// mini-MKV. `None` when no candidate qualifies.
#[allow(clippy::too_many_arguments)] // the muxer's inputs, passed through from the caller that gathered them
fn mini_mkv_from_candidates<R: Read + Seek>(
    r: &mut R,
    map: &SegmentMap,
    candidates: &[u64],
    video_track: Option<u64>,
    info: &[u8],
    info_hlen: usize,
    tracks: &[u8],
    rotation: Option<u32>,
) -> Option<(Vec<u8>, Option<u32>)> {
    for &candidate_abs in candidates {
        let Some((cluster_hlen, mut cluster)) = read_cluster(r, map, candidate_abs) else {
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
        let mut info = info.to_vec();
        zero_child(&mut info, info_hlen, ID_DURATION);
        return Some((build_mini_mkv(&map.ebml, &info, tracks, &cluster), rotation));
    }
    None
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
    if let Some(abs) = vp9_cued_cluster(r, &map, video_track, fraction) {
        candidates.push(abs);
    }
    if let Some(first) = map.first_cluster {
        if !candidates.contains(&first) {
            candidates.push(first);
        }
    }

    vp9_keyframe_in_clusters(r, &map, &candidates, video_track)
}

/// The absolute Cluster position of the Cues entry nearest `fraction`, read from the file's
/// own Cues and Info elements (both read even when one turns out unusable, matching the
/// caller's original evaluation) — the representative mid-video candidate. `None` (so the
/// caller falls back to the first Cluster) when either element is missing or the lookup fails.
fn vp9_cued_cluster<R: Read + Seek>(
    r: &mut R,
    map: &SegmentMap,
    video_track: u64,
    fraction: f64,
) -> Option<u64> {
    let cues_pos = map.cues?;
    let info_pos = map.info?;
    let cues_elem = read_element_full(r, cues_pos, CUES_MAX, ID_CUES);
    let info_elem = read_element_full(r, info_pos, META_MAX, ID_INFO);
    let (_, cues_hlen, cues) = cues_elem?;
    let (_, info_hlen, info) = info_elem?;
    let rel = cue_cluster_position(
        &cues[cues_hlen..],
        &info[info_hlen..],
        Some(video_track),
        fraction,
    )?;
    map.seg_data.checked_add(rel)
}

/// The first VP9 keyframe among the candidate Clusters, in order; a Cluster past the file
/// end, one that cannot be read, or one holding no keyframe for the video track is skipped.
fn vp9_keyframe_in_clusters<R: Read + Seek>(
    r: &mut R,
    map: &SegmentMap,
    candidates: &[u64],
    video_track: u64,
) -> Option<Vec<u8>> {
    for &cluster_abs in candidates {
        if cluster_abs >= map.total {
            continue;
        }
        let Some((chlen, cluster)) = read_cluster(r, map, cluster_abs) else {
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

/// The `profile_idc` of the video track's H.264 decoder configuration. Matroska stores the
/// same `AVCDecoderConfigurationRecord` an MP4 keeps in `avcC` as the TrackEntry's
/// `CodecPrivate`, so byte 1 is `AVCProfileIndication` here too. `None` for a non-Matroska
/// source, a track whose CodecID is not `V_MPEG4/ISO/AVC`, or a missing / short
/// CodecPrivate. Reads the EBML head + Tracks only. The twin of
/// [`crate::mp4::h264_profile_idc`], feeding [`crate::vcodec::mf_undecodable_reason`]
/// (issue #35).
pub fn h264_profile_idc<R: Read + Seek>(r: &mut R) -> Option<u8> {
    let map = segment_map(r)?;
    let (_, hlen, tracks) = read_element_full(r, map.tracks?, META_MAX, ID_TRACKS)?;
    let tracks_body = &tracks[hlen..];
    if video_track_codec(tracks_body).as_deref() != Some("V_MPEG4/ISO/AVC") {
        return None;
    }
    let entry = video_track_entry(tracks_body)?;
    let idc = children(entry)
        .find_map(|(cid, _, cd)| (cid == ID_CODEC_PRIVATE).then(|| cd.get(1).copied())?);
    idc
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

// ---------------------------------------------------------------------------------------------
// EBML slice parsing (over already-buffered elements)
// ---------------------------------------------------------------------------------------------

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

#[cfg(test)]
mod tests;
