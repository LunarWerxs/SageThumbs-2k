use super::*;
use std::io::Cursor;

// --- Synthetic FLV construction ----------------------------------------------------------

/// FLV file: 9-byte header + PreviousTagSize0 + the given tag bytes.
fn flv_file(tags: &[Vec<u8>]) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(b"FLV\x01\x05"); // version 1, audio+video flags
    f.extend_from_slice(&9u32.to_be_bytes()); // DataOffset
    f.extend_from_slice(&0u32.to_be_bytes()); // PreviousTagSize0
    for t in tags {
        f.extend_from_slice(t);
    }
    f
}

/// One FLV tag: header + payload + PreviousTagSize.
fn tag(tag_type: u8, timestamp: u32, payload: &[u8]) -> Vec<u8> {
    let mut t = Vec::with_capacity(15 + payload.len());
    t.push(tag_type);
    t.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..4]); // u24 DataSize
    t.extend_from_slice(&timestamp.to_be_bytes()[1..4]); // u24 Timestamp
    t.push(0); // TimestampExtended
    t.extend_from_slice(&[0, 0, 0]); // StreamID
    t.extend_from_slice(payload);
    t.extend_from_slice(&((11 + payload.len()) as u32).to_be_bytes());
    t
}

/// AVC video tag payload: FrameType|CodecID, AVCPacketType, CompositionTime, body.
fn avc_payload(frame_type: u8, codec_id: u8, packet_type: u8, body: &[u8]) -> Vec<u8> {
    let mut p = vec![(frame_type << 4) | codec_id, packet_type, 0, 0, 0];
    p.extend_from_slice(body);
    p
}

/// Bit-pack an SPS from (value, bit-width) pairs, MSB-first, stop-bit + padding added.
fn pack_bits(fields: &[(u32, u32)]) -> Vec<u8> {
    let mut bits = Vec::new();
    for &(v, n) in fields {
        for i in (0..n).rev() {
            bits.push((v >> i) & 1);
        }
    }
    bits.push(1); // rbsp_stop_one_bit
    while bits.len() % 8 != 0 {
        bits.push(0);
    }
    bits.chunks(8)
        .map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b as u8))
        .collect()
}

/// ue(v) as (value, width) for `pack_bits`.
fn ue_bits(v: u32) -> (u32, u32) {
    let code = v + 1;
    let len = 32 - code.leading_zeros();
    (code, 2 * len - 1)
}

/// A baseline-profile SPS for a `width`×`height` frame (multiples of 16, no cropping).
fn synthetic_sps(width_mbs: u32, height_mbs: u32) -> Vec<u8> {
    let mut rbsp = pack_bits(&[
        (66, 8),    // profile_idc: baseline — skips the chroma/scaling branch
        (0, 8),     // constraint flags
        (30, 8),    // level_idc
        ue_bits(0), // seq_parameter_set_id
        ue_bits(0), // log2_max_frame_num_minus4
        ue_bits(2), // pic_order_cnt_type = 2 (no extra fields)
        ue_bits(1), // max_num_ref_frames
        (0, 1),     // gaps_in_frame_num_value_allowed
        ue_bits(width_mbs - 1),
        ue_bits(height_mbs - 1),
        (1, 1), // frame_mbs_only
        (0, 1), // direct_8x8_inference
        (0, 1), // frame_cropping_flag
        (0, 1), // vui_parameters_present
    ]);
    let mut nal = vec![0x67]; // NAL header: type 7 (SPS)
    nal.append(&mut rbsp);
    nal
}

/// AVCDecoderConfigurationRecord wrapping one SPS (no PPS needed for the parse).
fn synthetic_avcc(sps: &[u8]) -> Vec<u8> {
    let mut c = vec![
        1,    // configurationVersion
        66,   // AVCProfileIndication
        0,    // profile_compatibility
        30,   // AVCLevelIndication
        0xFF, // lengthSizeMinusOne = 3 (4-byte lengths)
        0xE1, // numOfSequenceParameterSets = 1
    ];
    c.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    c.extend_from_slice(sps);
    c.push(0); // numOfPictureParameterSets = 0
    c
}

fn h264_flv(width_mbs: u32, height_mbs: u32) -> Vec<u8> {
    let avcc = synthetic_avcc(&synthetic_sps(width_mbs, height_mbs));
    let keyframe = [0u32.to_be_bytes().as_slice(), &[0x65, 0xAA, 0xBB]].concat();
    flv_file(&[
        tag(18, 0, b"\x02\x00\x0AonMetaData"), // script tag ahead of the video
        tag(9, 0, &avc_payload(1, 7, 0, &avcc)), // AVC sequence header
        tag(8, 0, &[0xAF, 0x00, 0x12]),        // an audio tag in between
        tag(9, 0, &avc_payload(1, 7, 1, &keyframe)), // the keyframe
    ])
}

// --- The happy path ----------------------------------------------------------------------

#[test]
fn h264_flv_yields_a_structurally_valid_mini_mp4() {
    let flv = h264_flv(4, 3); // 64×48
    let mini = keyframe_mini_mp4(&mut Cursor::new(&flv)).expect("mini-mp4 from H.264 FLV");
    assert_eq!(&mini[4..8], b"ftyp");
    for magic in [b"moov", b"avc1", b"avcC", b"mdat"] {
        assert!(
            mini.windows(4).any(|w| w == magic),
            "mini-mp4 missing {}",
            String::from_utf8_lossy(magic)
        );
    }
    // The keyframe bytes must be the mdat payload, verbatim.
    let mdat = mini.windows(4).position(|w| w == b"mdat").unwrap();
    assert_eq!(&mini[mdat + 4..], &[0, 0, 0, 0, 0x65, 0xAA, 0xBB]);
}

#[test]
fn sps_geometry_is_parsed() {
    let avcc = synthetic_avcc(&synthetic_sps(40, 23)); // 640×368
    assert_eq!(sps_dims(&avcc), Some((640, 368)));
}

#[test]
fn a_keyframe_before_the_sequence_header_is_skipped() {
    let avcc = synthetic_avcc(&synthetic_sps(4, 4));
    let keyframe = [0u32.to_be_bytes().as_slice(), &[0x65, 0x01]].concat();
    let flv = flv_file(&[
        tag(9, 0, &avc_payload(1, 7, 1, &keyframe)), // NALU first — unusable yet
        tag(9, 0, &avc_payload(1, 7, 0, &avcc)),     // then the config
        tag(9, 33, &avc_payload(2, 7, 1, &[0, 0, 0, 1, 0x41])), // inter frame — not sync
        tag(9, 66, &avc_payload(1, 7, 1, &keyframe)), // the first usable keyframe
    ]);
    assert!(keyframe_mini_mp4(&mut Cursor::new(&flv)).is_some());
}

// --- Adversarial cases: each must return None, never panic or hang -----------------------

#[test]
fn non_avc_codecs_return_none() {
    for codec_id in [2u8, 4, 5, 6] {
        // Sorenson Spark, VP6, VP6A, Screen 2
        let flv = flv_file(&[tag(9, 0, &avc_payload(1, codec_id, 0, &[0x11, 0x22]))]);
        assert_eq!(
            keyframe_mini_mp4(&mut Cursor::new(&flv)),
            None,
            "codec id {codec_id} must be declined"
        );
    }
}

#[test]
fn audio_only_flv_returns_none() {
    let flv = flv_file(&[
        tag(8, 0, &[0xAF, 0x00, 0x12, 0x34]),
        tag(8, 23, &[0xAF, 0x01, 0x56]),
    ]);
    assert_eq!(keyframe_mini_mp4(&mut Cursor::new(&flv)), None);
}

#[test]
fn zero_size_tags_cannot_stall_the_walk() {
    // A run of zero-DataSize tags followed by real video: the fixed 15-byte advance
    // must carry the walk over them (and the test finishing at all proves no hang).
    let mut tags: Vec<Vec<u8>> = (0..64).map(|i| tag(8, i, &[])).collect();
    let avcc = synthetic_avcc(&synthetic_sps(4, 4));
    let keyframe = [0u32.to_be_bytes().as_slice(), &[0x65]].concat();
    tags.push(tag(9, 0, &avc_payload(1, 7, 0, &avcc)));
    tags.push(tag(9, 40, &avc_payload(1, 7, 1, &keyframe)));
    assert!(keyframe_mini_mp4(&mut Cursor::new(&flv_file(&tags))).is_some());
}

/// The tag-count cap now fires at MAX_TAGS (4,096), not the old 200,000: a run of
/// `MAX_TAGS + 100` audio tags ahead of a real keyframe pushes the walk over the NEW
/// cap while staying comfortably under the old one, so this proves the lower value is
/// actually enforced rather than merely declared.
#[test]
fn tag_count_cap_fires_at_the_new_lower_threshold() {
    // A compile-time check (clippy correctly flags a runtime assert on a const as
    // pointless): MAX_TAGS must stay a meaningfully small cap, not creep back toward
    // the old 200k.
    const _: () = assert!(MAX_TAGS < 10_000);
    let avcc = synthetic_avcc(&synthetic_sps(4, 3));
    let keyframe = [0u32.to_be_bytes().as_slice(), &[0x65, 0xAA, 0xBB]].concat();
    let mut tags: Vec<Vec<u8>> = (0..MAX_TAGS + 100)
        .map(|i| tag(8, i, &[0xAF, 0x00]))
        .collect();
    tags.push(tag(9, 0, &avc_payload(1, 7, 0, &avcc)));
    tags.push(tag(9, 40, &avc_payload(1, 7, 1, &keyframe)));
    assert_eq!(
        keyframe_mini_mp4(&mut Cursor::new(&flv_file(&tags))),
        None,
        "a keyframe past MAX_TAGS audio tags must be capped, not found"
    );
}

/// The same construction with the video tags brought back under the cap succeeds —
/// pinning that the previous test's `None` is the cap firing, not some other rejection.
#[test]
fn tag_count_just_under_the_cap_still_finds_the_keyframe() {
    let avcc = synthetic_avcc(&synthetic_sps(4, 3));
    let keyframe = [0u32.to_be_bytes().as_slice(), &[0x65, 0xAA, 0xBB]].concat();
    let mut tags: Vec<Vec<u8>> = (0..MAX_TAGS - 10)
        .map(|i| tag(8, i, &[0xAF, 0x00]))
        .collect();
    tags.push(tag(9, 0, &avc_payload(1, 7, 0, &avcc)));
    tags.push(tag(9, 40, &avc_payload(1, 7, 1, &keyframe)));
    assert!(keyframe_mini_mp4(&mut Cursor::new(&flv_file(&tags))).is_some());
}

#[test]
fn tag_size_past_eof_returns_none() {
    // DataSize claims more bytes than the file holds — truncated mid-tag.
    let mut flv = flv_file(&[tag(
        9,
        0,
        &avc_payload(1, 7, 0, &[1, 66, 0, 30, 0xFF, 0xE1]),
    )]);
    // Rewrite the first tag's DataSize (u24 at header+4+1) to a lie.
    flv[14] = 0xFF;
    flv[15] = 0xFF;
    assert_eq!(keyframe_mini_mp4(&mut Cursor::new(&flv)), None);
}

#[test]
fn max_u24_declared_size_returns_none() {
    // The FLV counterpart of "a declared sample size of 0xFFFFFFFF": DataSize is 24-bit,
    // so 0xFFFFFF is the format's maximum lie. Bounds-checked, not allocated.
    let mut t = vec![9u8, 0xFF, 0xFF, 0xFF]; // type 9, DataSize u24::MAX
    t.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]); // timestamp + stream id
    t.extend_from_slice(&avc_payload(1, 7, 0, &[1, 66, 0, 30, 0xFF, 0xE1]));
    let flv = flv_file(&[t]);
    assert_eq!(keyframe_mini_mp4(&mut Cursor::new(&flv)), None);
}

#[test]
fn truncations_and_junk_return_none() {
    let flv = h264_flv(4, 4);
    for n in 0..flv.len() {
        // Every prefix: legal outcomes are Some (once the keyframe tag fits) or None,
        // never a panic. The interesting assertions are the early cuts:
        let _ = keyframe_mini_mp4(&mut Cursor::new(&flv[..n]));
    }
    assert_eq!(keyframe_mini_mp4(&mut Cursor::new(&b"FLV"[..])), None);
    assert_eq!(keyframe_mini_mp4(&mut Cursor::new(&[0u8; 64][..])), None);
    assert_eq!(
        keyframe_mini_mp4(&mut Cursor::new(&b"not an flv at all"[..])),
        None
    );
    // A header whose DataOffset points far past anything sane.
    let mut bad = h264_flv(4, 4);
    bad[5] = 0xFF;
    bad[6] = 0xFF;
    assert_eq!(keyframe_mini_mp4(&mut Cursor::new(&bad)), None);
}

#[test]
fn malformed_avcc_returns_none() {
    // Config version != 1.
    let flv = flv_file(&[tag(
        9,
        0,
        &avc_payload(1, 7, 0, &[2, 66, 0, 30, 0xFF, 0xE1, 0]),
    )]);
    assert_eq!(keyframe_mini_mp4(&mut Cursor::new(&flv)), None);
    // Config with zero SPS entries — the keyframe can never mux.
    let cfg = [1u8, 66, 0, 30, 0xFF, 0xE0, 0];
    let keyframe = [0u32.to_be_bytes().as_slice(), &[0x65]].concat();
    let flv = flv_file(&[
        tag(9, 0, &avc_payload(1, 7, 0, &cfg)),
        tag(9, 40, &avc_payload(1, 7, 1, &keyframe)),
    ]);
    assert_eq!(keyframe_mini_mp4(&mut Cursor::new(&flv)), None);
}

/// The corpus `sample.flv` is Sorenson Spark (codec id 2) — the MP4-remux path must
/// still decline it cleanly (it now renders via [`flash_frame`]'s out-of-process tier
/// instead), and the probe must name its codec. Skips when the dev corpus isn't
/// present (CI).
#[test]
fn corpus_sorenson_flv_declines_the_mp4_remux_path() {
    let path = crate::testcorpus::dir().join("sample.flv");
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("corpus_sorenson_flv: no sample.flv — skipping");
        return;
    };
    assert_eq!(keyframe_mini_mp4(&mut Cursor::new(&bytes)), None);
    assert_eq!(video_codec_id(&mut Cursor::new(&bytes)), Some(2));
    assert!(matches!(
        scan_flash_keyframe(&bytes),
        FlashScan::Keyframe(FlashCodec::Sorenson, _)
    ));
}

/// The corpus `sample-h264.flv` is a REAL H.264 FLV, and it must take the in-process
/// mini-MP4 remux — never the helper process.
///
/// `real_h264_flv_round_trips_through_mediafoundation` below already exercises this path,
/// but it BUILDS its input: it lifts a genuine avcC and keyframe out of `sample.mp4` and
/// wraps them in an FLV this test file wrote. That proves the muxer and the Media
/// Foundation hand-off, and it cannot prove we read the tag layout a real Flash encoder
/// emits — the half that faces users, and the codec behind the original "FLVs are blank"
/// report. So this reads a file made by someone else's encoder.
///
/// The `OtherCodec(7)` assertion is the load-bearing one: it is what keeps H.264 OFF the
/// spawned-child tier. If it ever became a `Keyframe`, every H.264 FLV — the commonest
/// kind — would cost a process per thumbnail while still looking perfectly correct.
#[test]
fn corpus_h264_flv_remuxes_in_process() {
    let path = crate::testcorpus::dir().join("sample-h264.flv");
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("corpus_h264_flv: no sample-h264.flv — skipping");
        return;
    };
    assert_eq!(video_codec_id(&mut Cursor::new(&bytes)), Some(7));
    assert!(
        matches!(scan_flash_keyframe(&bytes), FlashScan::OtherCodec(7)),
        "H.264 must be deferred by the Flash scanner, not decoded out of process"
    );
    let mp4 = keyframe_mini_mp4(&mut Cursor::new(&bytes))
        .expect("a real H.264 FLV must remux to a mini-MP4");
    assert!(mp4.len() > 64, "mini-MP4 implausibly small: {}", mp4.len());
    assert_eq!(&mp4[4..8], b"ftyp", "mini-MP4 must start with an ftyp box");
}

// --- The Flash-codec (VP6/Sorenson) probe + scan -----------------------------------------

#[test]
fn flash_scan_finds_the_first_sorenson_keyframe() {
    let body = [0x08u8; 32];
    let flv = flv_file(&[
        tag(18, 0, b"\x02\x00\x0AonMetaData"),
        tag(8, 0, &[0xAF, 0x00, 0x12]), // audio ahead of the video
        tag(9, 0, &{
            let mut p = vec![(2 << 4) | 2]; // inter frame first — must be skipped
            p.extend_from_slice(&[0xEE; 8]);
            p
        }),
        tag(9, 40, &{
            let mut p = vec![(1 << 4) | 2]; // the keyframe
            p.extend_from_slice(&body);
            p
        }),
    ]);
    match scan_flash_keyframe(&flv) {
        FlashScan::Keyframe(FlashCodec::Sorenson, payload) => assert_eq!(payload, body),
        _ => panic!("expected a Sorenson keyframe"),
    }
    assert_eq!(video_codec_id(&mut Cursor::new(&flv)), Some(2));
}

#[test]
fn flash_scan_reports_vp6_and_defers_h264() {
    let vp6 = flv_file(&[tag(9, 0, &{
        let mut p = vec![(1 << 4) | 4, 0x21]; // adjustment byte then frame data
        p.extend_from_slice(&[0x55; 16]);
        p
    })]);
    match scan_flash_keyframe(&vp6) {
        FlashScan::Keyframe(FlashCodec::Vp6, payload) => {
            assert_eq!(payload[0], 0x21); // the adjustment byte stays for the decoder
            assert_eq!(payload.len(), 17);
        }
        _ => panic!("expected a VP6 keyframe"),
    }
    let h264 = h264_flv(4, 4);
    assert!(matches!(
        scan_flash_keyframe(&h264),
        FlashScan::OtherCodec(7)
    ));
    assert_eq!(video_codec_id(&mut Cursor::new(&h264)), Some(7));
}

#[test]
fn flash_scan_declines_junk_cleanly() {
    assert!(matches!(scan_flash_keyframe(&[]), FlashScan::NoVideo));
    assert!(matches!(
        scan_flash_keyframe(b"not an flv at all, sorry"),
        FlashScan::NoVideo
    ));
    // Audio-only: no video tag ever arrives.
    let audio = flv_file(&[tag(8, 0, &[0xAF, 0x00, 0x12, 0x34])]);
    assert!(matches!(scan_flash_keyframe(&audio), FlashScan::NoVideo));
    // Every truncation must scan without panicking.
    let whole = flv_file(&[tag(9, 0, &[(1 << 4) | 2, 0xAA, 0xBB])]);
    for n in 0..whole.len() {
        let _ = scan_flash_keyframe(&whole[..n]);
        let _ = video_codec_id(&mut Cursor::new(&whole[..n]));
    }
}

/// With no `st2k.exe` next to the test binary, the out-of-process tier must decline
/// cleanly (that is also the DLL-only / feature-less install behaviour). H.264 input
/// must not even attempt a spawn.
#[test]
fn flash_frame_declines_cleanly_without_the_helper() {
    let sorenson = flv_file(&[tag(9, 0, &{
        let mut p = vec![(1 << 4) | 2];
        p.extend_from_slice(&[0x08; 16]);
        p
    })]);
    assert!(flash_frame(&mut Cursor::new(&sorenson)).is_none());
    assert!(flash_frame(&mut Cursor::new(&h264_flv(4, 4))).is_none());
    assert!(flash_frame(&mut Cursor::new(&b"junk"[..])).is_none());
}

/// End-to-end through Media Foundation: take the REAL avcC + keyframe out of the corpus
/// `sample.mp4`'s mini-MP4 (H.264), wrap them in a synthetic FLV, run the FLV path, and
/// decode the result. Proves the FLV → mini-MP4 → MF chain on genuine H.264 bytes
/// without committing a video fixture. Skips when no corpus sample is available.
#[test]
fn real_h264_flv_round_trips_through_mediafoundation() {
    let path = crate::testcorpus::dir().join("sample.mp4");
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("real_h264_flv_round_trips: no sample.mp4 — skipping");
        return;
    };
    let (mini, _rotation) = crate::mp4::keyframe_mini_mp4(&mut Cursor::new(&bytes), 0.30)
        .expect("mini-mp4 from the corpus sample");
    // Pull the avcC payload (its box header precedes the fourcc) and the mdat payload
    // (the keyframe sample) back out of the known-simple mini-MP4 layout.
    let avcc_at = mini
        .windows(4)
        .position(|w| w == b"avcC")
        .expect("corpus sample should be H.264");
    let avcc_size = u32::from_be_bytes(mini[avcc_at - 4..avcc_at].try_into().unwrap()) as usize;
    let avcc = &mini[avcc_at + 4..avcc_at - 4 + avcc_size];
    let mdat_at = mini
        .windows(4)
        .position(|w| w == b"mdat")
        .expect("mini-mp4 has an mdat");
    let keyframe = &mini[mdat_at + 4..];

    let flv = flv_file(&[
        tag(18, 0, b"\x02\x00\x0AonMetaData"),
        tag(9, 0, &avc_payload(1, 7, 0, avcc)),
        tag(9, 0, &avc_payload(1, 7, 1, keyframe)),
    ]);
    let mini2 =
        keyframe_mini_mp4(&mut Cursor::new(&flv)).expect("mini-mp4 from the synthesized H.264 FLV");
    let frame = crate::video::frame_from_bytes(&mini2)
        .expect("Media Foundation should decode the FLV-sourced keyframe");
    assert!(frame.width() > 0 && frame.height() > 0);
    eprintln!(
        "real_h264_flv_round_trips: flv {} bytes → mini {} bytes → frame {}x{}",
        flv.len(),
        mini2.len(),
        frame.width(),
        frame.height()
    );
}
