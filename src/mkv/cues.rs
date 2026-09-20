//! Cues and the seek head: where the cluster that holds a wanted keyframe starts.

use super::*;

/// The Segment-relative position of the Cluster holding the keyframe nearest `fraction` of
/// the running time, from an in-memory Cues body. Shared by [`keyframe_mini_mkv`] (which
/// wraps that cluster for Media Foundation) and [`vp9_keyframe`] (which pulls the raw block
/// out of it), so the two can't disagree about WHICH frame represents the file.
pub(super) fn cue_cluster_position(
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

/// One CueTrackPositions child: `(this track's position if it's the video track, this track's
/// cluster position as the any-track fallback)`.
pub(super) fn cue_track_position(
    cd: &[u8],
    video_track: Option<u64>,
) -> (Option<u64>, Option<u64>) {
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
pub(super) fn parse_cue_point(cp: &[u8], video_track: Option<u64>) -> Option<(u64, u64)> {
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
pub(super) fn cue_points(cues_data: &[u8], video_track: Option<u64>) -> Vec<(u64, u64)> {
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
pub(super) fn zero_child(elem: &mut [u8], elem_hlen: usize, target: u64) {
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
pub(super) fn seek_lookup(seekhead: &[u8], target_id: u64) -> Option<u64> {
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
