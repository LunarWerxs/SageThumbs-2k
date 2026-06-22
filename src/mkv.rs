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

const TRACK_TYPE_VIDEO: u64 = 1;

// Sanity caps on the bounded elements we pull into memory.
const META_MAX: u64 = 8 * 1024 * 1024; // EBML header / Info / Tracks
const CUES_MAX: u64 = 32 * 1024 * 1024; // the index
const CLUSTER_MAX: u64 = 96 * 1024 * 1024; // one cluster (≤ a few seconds of 4K)

/// Build a one-cluster mini-MKV for the keyframe nearest `fraction` of the running time, for
/// [`crate::video::frame_from_bytes`]. `None` if the source isn't a Cues-indexed Matroska/WebM
/// (caller falls back to the bounded head-prefix tier).
pub fn keyframe_mini_mkv<R: Read + Seek>(r: &mut R, fraction: f64) -> Option<Vec<u8>> {
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
    let seg_end = if sunk { total } else { (seg_data + ssize).min(total) };

    // Front-of-segment walk: capture SeekHead/Info/Tracks (and Cues if it happens to be up
    // front), stopping at the first Cluster — we never scan the cluster body.
    let mut seekhead: Option<Vec<u8>> = None;
    let mut info_pos = None;
    let mut tracks_pos = None;
    let mut cues_pos = None;
    let mut p = seg_data;
    for _ in 0..64 {
        if p + 2 > seg_end {
            break;
        }
        let (eid, esize, ehlen, eunk) = header_at(r, p)?;
        match eid {
            ID_SEEKHEAD if esize <= META_MAX => {
                seekhead = read_full_at(r, p + ehlen, esize)
            }
            ID_INFO => info_pos = Some(p),
            ID_TRACKS => tracks_pos = Some(p),
            ID_CUES => cues_pos = Some(p),
            ID_CLUSTER => break,
            _ => {}
        }
        if eunk {
            break; // can't skip an unknown-size element
        }
        p = p.checked_add(ehlen + esize)?;
        if info_pos.is_some() && tracks_pos.is_some() && cues_pos.is_some() {
            break;
        }
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
    }

    let (_, info_hlen, mut info) = read_element_full(r, info_pos?, META_MAX, ID_INFO)?;
    let (_, tracks_hlen, tracks) = read_element_full(r, tracks_pos?, META_MAX, ID_TRACKS)?;
    let (_, cues_hlen, cues) = read_element_full(r, cues_pos?, CUES_MAX, ID_CUES)?;

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
    let (_, cluster_hlen, mut cluster) = read_element_full(r, cluster_abs, CLUSTER_MAX, ID_CLUSTER)?;
    zero_child(&mut cluster, cluster_hlen, ID_CLUSTER_TIMECODE);
    zero_child(&mut info, info_hlen, ID_DURATION);

    Some(build_mini_mkv(&ebml, &info, &tracks, &cluster))
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
    let mask = 0xFFu8 >> sz_len;
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
    let mask = 0xFFu8 >> len;
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
    data.iter().take(8).fold(0u64, |acc, &b| (acc << 8) | b as u64)
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

    /// End-to-end: parse a real MKV (path in `ST2K_TEST_MKV`) into a one-cluster mini-MKV and
    /// decode it through Media Foundation. Skipped when the env var isn't set / file is absent,
    /// so CI stays green without an adult-content fixture in the repo.
    #[test]
    fn real_mkv_round_trips_through_mediafoundation() {
        let Some(path) = std::env::var("ST2K_TEST_MKV").ok().filter(|p| Path::new(p).is_file())
        else {
            eprintln!("real_mkv_round_trips: ST2K_TEST_MKV unset / missing — skipping");
            return;
        };
        let bytes = std::fs::read(&path).expect("read sample mkv");
        let mini = keyframe_mini_mkv(&mut Cursor::new(&bytes), 0.30)
            .expect("build mini-mkv from real sample");
        assert!(mini[0..4] == [0x1A, 0x45, 0xDF, 0xA3], "starts with EBML header");
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
