#![cfg(test)]

use super::*;
mod rotation;
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

/// Wrap a Segment `body` in an EBML header plus a definite-size Segment element.
fn segment_file(body: &[u8]) -> Vec<u8> {
    let mut file = elem(ID_EBML, &[0u8; 4]);
    file.extend_from_slice(&elem(ID_SEGMENT, body));
    file
}

/// A Tracks element holding one video TrackEntry for `codec`, with an optional track number.
fn video_tracks(codec: &[u8], number: Option<u64>) -> Vec<u8> {
    let mut entry = Vec::new();
    if let Some(n) = number {
        entry.extend_from_slice(&elem(ID_TRACK_NUMBER, &[n as u8]));
    }
    entry.extend_from_slice(&elem(ID_TRACK_TYPE, &[TRACK_TYPE_VIDEO as u8]));
    entry.extend_from_slice(&elem(ID_CODEC_ID, codec));
    elem(ID_TRACKS, &elem(ID_TRACK_ENTRY, &entry))
}

/// The codec ID and attached cover `file` must yield, read in that order.
fn assert_codec_and_cover(file: &[u8], codec: &str, cover: &[u8]) {
    let mut cur = Cursor::new(file);
    assert_eq!(video_codec_id(&mut cur).as_deref(), Some(codec));
    assert_eq!(attached_cover(&mut cur).as_deref(), Some(cover));
}

#[test]
fn codec_id_and_attached_cover_from_synthetic_mkv() {
    let tracks = video_tracks(b"V_MPEGH/ISO/HEVC", Some(1));
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
    let file = segment_file(&[tracks, attachments].concat());

    assert_codec_and_cover(&file, "V_MPEGH/ISO/HEVC", b"JPEGDATA");
}

/// Issue #35's Matroska half: the H.264 profile comes out of CodecPrivate byte 1, only
/// for an AVC track, and a missing or short CodecPrivate is a shrug, never a refusal.
#[test]
fn h264_profile_idc_reads_codec_private_for_avc_tracks_only() {
    let file_with = |codec: &[u8], private: Option<&[u8]>| {
        let mut entry = [
            elem(ID_TRACK_NUMBER, &[1]),
            elem(ID_TRACK_TYPE, &[TRACK_TYPE_VIDEO as u8]),
            elem(ID_CODEC_ID, codec),
        ]
        .concat();
        if let Some(p) = private {
            entry.extend_from_slice(&elem(ID_CODEC_PRIVATE, p));
        }
        let tracks = elem(ID_TRACKS, &elem(ID_TRACK_ENTRY, &entry));
        segment_file(&tracks)
    };
    let avcc_444 = [1u8, 244, 0, 31, 0xFF, 0xE0, 0x00];
    let probe = |file: Vec<u8>| h264_profile_idc(&mut Cursor::new(file));
    assert_eq!(
        probe(file_with(b"V_MPEG4/ISO/AVC", Some(&avcc_444))),
        Some(244)
    );
    assert_eq!(
        probe(file_with(b"V_MPEG4/ISO/AVC", Some(&[1, 100, 0, 31]))),
        Some(100)
    );
    assert_eq!(
        probe(file_with(b"V_MPEGH/ISO/HEVC", Some(&avcc_444))),
        None,
        "not H.264"
    );
    assert_eq!(
        probe(file_with(b"V_MPEG4/ISO/AVC", None)),
        None,
        "no CodecPrivate"
    );
    assert_eq!(
        probe(file_with(b"V_MPEG4/ISO/AVC", Some(&[1]))),
        None,
        "short CodecPrivate"
    );
    // And the gate itself, through the shared entry point.
    let reason = crate::vcodec::mf_undecodable_reason(&mut Cursor::new(file_with(
        b"V_MPEG4/ISO/AVC",
        Some(&avcc_444),
    )))
    .expect("4:4:4 in Matroska must be refused too");
    assert!(reason.contains("High 4:4:4 Predictive"), "{reason}");
}

/// Cues-up-front layout with NO SeekHead: Info, Tracks, Cues, Attachments, Cluster.
/// The walk used to break as soon as info+tracks+cues were all found, skipping the
/// Attachments element sitting right after Cues — losing the cover art of any file
/// whose SeekHead is absent or doesn't list Attachments (mkvpropedit-appended covers).
#[test]
fn attachments_after_cues_survive_without_a_seekhead() {
    let tracks = video_tracks(b"V_MPEGH/ISO/HEVC", None);
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
    let file = segment_file(&body);

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
    let tracks = video_tracks(b"V_AV1", None);
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
    let file = segment_file(&body);

    assert_codec_and_cover(&file, "V_AV1", b"PNGDATA");
}

// --- vp9_keyframe: the raw-block extraction for the out-of-process VP9 decoder -------

/// A minimal VP9 Matroska: Tracks (track 1 = V_VP9 video) + one Cluster of blocks.
fn vp9_mkv(cluster_children: &[Vec<u8>]) -> Vec<u8> {
    let tracks = video_tracks(b"V_VP9", Some(1));
    let cluster = elem(
        ID_CLUSTER,
        &[
            elem(ID_CLUSTER_TIMECODE, &[0]).as_slice(),
            &cluster_children.concat(),
        ]
        .concat(),
    );
    segment_file(&[tracks, cluster].concat())
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
    let path = crate::testcorpus::dir().join("sample-vp9p2.webm");
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
    let tracks = video_tracks(b"V_VP9", Some(1));
    let cluster_body = [
        elem(ID_CLUSTER_TIMECODE, &[0]),
        simple_block(0x80, &[0x86, 0x00, 0x42, 0x11, 0x22]),
    ]
    .concat();
    // Hand-built unknown-size Cluster header: the 4-byte Cluster ID + a 1-byte
    // all-ones size vint (0xFF), which `elem` (definite-size only) cannot produce.
    let mut cluster = vec![0x1F, 0x43, 0xB6, 0x75, 0xFF];
    cluster.extend_from_slice(&cluster_body);

    let file = segment_file(&[tracks, cluster].concat());

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

/// A SeekHead with a single Seek: the Cues element at Segment-relative byte `pos`.
fn seekhead_to_cues(pos: u16) -> Vec<u8> {
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
}

/// A Cues element with one CuePoint (time 0) pointing at Segment-relative byte `pos`.
fn cues_at(pos: u32) -> Vec<u8> {
    elem(
        ID_CUES,
        &elem(
            ID_CUE_POINT,
            &[
                elem(ID_CUE_TIME, &[0]),
                elem(
                    ID_CUE_TRACK_POSITIONS,
                    &[
                        elem(ID_CUE_TRACK, &[1]),
                        elem(ID_CUE_CLUSTER_POSITION, &pos.to_be_bytes()),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        ),
    )
}

/// Build a Segment: SeekHead (pointing at Cues), Info, Tracks, a "good" first Cluster
/// (a real video keyframe), a "bad" second Cluster (no keyframe — an inter block), then
/// Cues whose one entry points at the BAD cluster. `good_cluster_has_keyframe` swaps the
/// good cluster's block for another keyframe-less one, for the decline-path test.
/// Returns `(file_bytes, bad_cluster_segment_relative_position)`.
fn mini_mkv_two_clusters(good_cluster_has_keyframe: bool) -> (Vec<u8>, u32) {
    let tracks = video_tracks(b"V_VP9", Some(1));
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
    let bad_rel =
        (seekhead_to_cues(0).len() + info.len() + tracks.len() + cluster_good.len()) as u32;
    let cues_rel = bad_rel + cluster_bad.len() as u32;
    let cues = cues_at(bad_rel);
    let body = [
        seekhead_to_cues(cues_rel as u16),
        info,
        tracks,
        cluster_good,
        cluster_bad,
        cues,
    ]
    .concat();
    let file = segment_file(&body);
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
    let cluster_rel = (seekhead_to_cues(0).len() + info.len() + tracks.len()) as u32;
    let cues_rel = cluster_rel + cluster.len() as u32;
    let cues = cues_at(cluster_rel);
    let body = [
        seekhead_to_cues(cues_rel as u16),
        info,
        tracks,
        cluster,
        cues,
    ]
    .concat();
    let file = segment_file(&body);

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
    let (mini, _rotation) =
        keyframe_mini_mkv(&mut Cursor::new(&bytes), 0.30).expect("build mini-mkv from real sample");
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
    let tracks = video_tracks(b"V_AV1", None);
    let mut body = tracks;
    // Two bytes so the loop's `p + 2 > seg_end` pre-check doesn't just break on its own
    // before header_at ever runs — this must exercise header_at returning None, not the
    // ordinary "ran out of room" exit.
    body.push(0x00);
    body.push(0x00);
    let file = segment_file(&body);

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
    let tracks = video_tracks(b"V_AV1", None);
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
    let file = segment_file(&body);

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
