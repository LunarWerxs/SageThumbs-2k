//! Structurally valid synthetic seeds - MKV, MP4, FLV, WebM/VP9, WAV, AIFF, ASF - and the format-magic stubs.

// The EBML element encoder + the Matroska ID table below are `crate::mkv`'s own - reused here
// rather than kept as a second, driftable copy (mkv.rs's own doc comment on `elem` explains
// why: the class of bug this whole fuzz harness exists to catch is exactly a parser/encoder
// desync).
use crate::mkv::elem as ebml;
use crate::mkv::{
    ID_ATTACHED_FILE, ID_ATTACHMENTS, ID_CLUSTER, ID_CLUSTER_TIMECODE, ID_CODEC_ID, ID_CUES,
    ID_CUE_CLUSTER_POSITION, ID_CUE_POINT, ID_CUE_TIME, ID_CUE_TRACK, ID_CUE_TRACK_POSITIONS,
    ID_DURATION, ID_EBML, ID_FILE_DATA, ID_FILE_MIME, ID_FILE_NAME, ID_INFO, ID_SEGMENT,
    ID_TIMECODE_SCALE, ID_TRACKS, ID_TRACK_ENTRY, ID_TRACK_NUMBER, ID_TRACK_TYPE,
};

/// A structurally valid Matroska file with the pieces every reader here walks: EBML header,
/// Segment, SeekHead, Info, Tracks (one HEVC video track), Cues, a Cluster, and Attachments
/// with a cover. Not a decodable video — a scaffold rich enough that mutations reach the
/// arithmetic-bearing code instead of bailing at the magic.
pub(super) fn synthetic_mkv() -> Vec<u8> {
    let info = ebml(
        ID_INFO,
        &[
            ebml(ID_TIMECODE_SCALE, &[0x0F, 0x42, 0x40]),
            ebml(ID_DURATION, &1000.0f32.to_be_bytes()),
        ]
        .concat(),
    );
    let tracks = ebml(
        ID_TRACKS,
        &ebml(
            ID_TRACK_ENTRY,
            &[
                ebml(ID_TRACK_NUMBER, &[1]),
                ebml(ID_TRACK_TYPE, &[1]),
                ebml(ID_CODEC_ID, b"V_MPEGH/ISO/HEVC"),
            ]
            .concat(),
        ),
    );
    let cues = ebml(
        ID_CUES,
        &ebml(
            ID_CUE_POINT,
            &[
                ebml(ID_CUE_TIME, &[0]),
                ebml(
                    ID_CUE_TRACK_POSITIONS,
                    &[
                        ebml(ID_CUE_TRACK, &[1]),
                        ebml(ID_CUE_CLUSTER_POSITION, &[0]),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        ),
    );
    let cluster = ebml(ID_CLUSTER, &ebml(ID_CLUSTER_TIMECODE, &[0]));
    let attachments = ebml(
        ID_ATTACHMENTS,
        &ebml(
            ID_ATTACHED_FILE,
            &[
                ebml(ID_FILE_NAME, b"cover.jpg"),
                ebml(ID_FILE_MIME, b"image/jpeg"),
                ebml(ID_FILE_DATA, b"\xFF\xD8\xFF\xE0JPEGISHDATA\xFF\xD9"),
            ]
            .concat(),
        ),
    );
    let body = [info, tracks, cues, cluster, attachments].concat();
    let mut file = ebml(ID_EBML, &[0x42, 0x82, 0x84, b'w', b'e', b'b', b'm']);
    file.extend_from_slice(&ebml(ID_SEGMENT, &body));
    file
}

/// Big-endian ISO-BMFF box: `size(4) type(4) payload`.
pub(super) fn mp4box(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = ((8 + payload.len()) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(typ);
    v.extend_from_slice(payload);
    v
}

/// Full box: `size(4) type(4) version(1) flags(3) payload`.
pub(super) fn mp4full(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut body = vec![0u8; 4];
    body.extend_from_slice(payload);
    mp4box(typ, &body)
}

pub(super) fn concat32(vals: &[u32]) -> Vec<u8> {
    vals.iter().flat_map(|v| v.to_be_bytes()).collect()
}

/// A structurally valid MP4 with a single indexed video track, laid out the way
/// `mp4::keyframe_mini_mp4`/`video_codec_fourcc` walk: ftyp, then moov ▸ trak ▸ mdia ▸
/// { mdhd, hdlr='vide', minf ▸ stbl ▸ { stsd(avc1 visual entry), stts, stsc, stco, stsz,
/// stss } }, then a small mdat the stco points at.
pub(super) fn synthetic_mp4() -> Vec<u8> {
    // mdhd v0: creation(4) modification(4) timescale(4) duration(4) lang(2) pre_defined(2).
    let mdhd = mp4full(b"mdhd", &concat32(&[0, 0, 1000, 100]));
    // hdlr: pre_defined(4) handler_type(4 = 'vide') reserved(12) name(1, nul).
    let mut hdlr_body = vec![0u8; 4];
    hdlr_body.extend_from_slice(b"vide");
    hdlr_body.extend_from_slice(&[0u8; 12]);
    hdlr_body.push(0);
    let hdlr = mp4full(b"hdlr", &hdlr_body);

    // VisualSampleEntry: standard fixed layout so width/height land at stsd[48],[50] and the
    // fourcc at stsd[20..24]. size(4) type(4) reserved(6) dri(2) pre(2) res(2) pre(12)
    // width(2) height(2) hres(4) vres(4) res(4) frame_count(2) compressorname(32) depth(2)
    // pre(2).
    let mut vse = Vec::new();
    vse.extend_from_slice(&[0u8; 6]); // reserved
    vse.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    vse.extend_from_slice(&[0u8; 2]); // pre_defined
    vse.extend_from_slice(&[0u8; 2]); // reserved
    vse.extend_from_slice(&[0u8; 12]); // pre_defined[3]
    vse.extend_from_slice(&64u16.to_be_bytes()); // width
    vse.extend_from_slice(&64u16.to_be_bytes()); // height
    vse.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // horiz dpi 72
    vse.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // vert dpi 72
    vse.extend_from_slice(&[0u8; 4]); // reserved
    vse.extend_from_slice(&1u16.to_be_bytes()); // frame_count
    vse.extend_from_slice(&[0u8; 32]); // compressorname
    vse.extend_from_slice(&24u16.to_be_bytes()); // depth
    vse.extend_from_slice(&0xFFFFu16.to_be_bytes()); // pre_defined -1
    let avc1 = mp4box(b"avc1", &vse);
    // stsd: version+flags(4) entry_count(4) then the entry.
    let mut stsd_body = 1u32.to_be_bytes().to_vec();
    stsd_body.extend_from_slice(&avc1);
    let stsd = mp4full(b"stsd", &stsd_body);

    let stts = mp4full(b"stts", &concat32(&[1, 4, 1000])); // 4 samples, 1000 ticks each
    let stsc = mp4full(b"stsc", &concat32(&[1, 1, 1, 1])); // one sample per chunk
    let stsz = mp4full(b"stsz", &concat32(&[8, 4])); // uniform 8-byte samples, 4 of them
    let stss = mp4full(b"stss", &concat32(&[1, 1])); // sample 1 is a sync sample

    // stco: 4 chunk offsets. Placeholder now; patched to the real mdat offset after layout.
    let stco = mp4full(b"stco", &concat32(&[4, 0, 0, 0, 0]));

    let stbl = mp4box(b"stbl", &[stsd, stts, stsc, stco, stsz, stss].concat());
    let minf = mp4box(b"minf", &stbl);
    let mdia = mp4box(b"mdia", &[mdhd, hdlr, minf].concat());
    let trak = mp4box(b"trak", &mdia);
    let moov = mp4box(b"moov", &trak);
    let ftyp = mp4box(b"ftyp", b"isom\0\0\x02\0isomiso2avc1mp41");

    let mut out = Vec::new();
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    let mdat_data_off = (out.len() + 8) as u32;
    // Patch the four stco offsets to point at consecutive 8-byte samples inside the mdat.
    let stco_pat = find_subslice(&out, b"stco").expect("stco present");
    let first = stco_pat + 8 + 4 + 4; // box header + ver/flags + entry_count
    for i in 0..4u32 {
        let off = (mdat_data_off + i * 8).to_be_bytes();
        out[first + i as usize * 4..first + i as usize * 4 + 4].copy_from_slice(&off);
    }
    out.extend_from_slice(&mp4box(b"mdat", &[0xABu8; 32]));
    out
}

/// A structurally valid H.264 FLV, the way `flv::keyframe_mini_mp4` walks it: header,
/// PreviousTagSize0, a script tag, an AVC sequence-header tag whose payload is an
/// AVCDecoderConfigurationRecord with a real baseline SPS, an audio tag, and a keyframe
/// NALU tag. Mutations reach the tag walk, the config parse, AND the Exp-Golomb SPS
/// geometry reader instead of bouncing off the "FLV" magic.
pub(super) fn synthetic_flv() -> Vec<u8> {
    fn tag(tag_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut t = vec![tag_type];
        t.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..4]); // u24 DataSize
        t.extend_from_slice(&[0u8; 7]); // timestamp(3) + ext(1) + stream id(3)
        t.extend_from_slice(payload);
        t.extend_from_slice(&((11 + payload.len()) as u32).to_be_bytes());
        t
    }
    let avcc = h264_avcc();
    let mut seq_payload = vec![0x17, 0x00, 0, 0, 0]; // keyframe|AVC, seq header, cts
    seq_payload.extend_from_slice(&avcc);
    let mut kf_payload = vec![0x17, 0x01, 0, 0, 0]; // keyframe|AVC, NALU, cts
    kf_payload.extend_from_slice(&[0, 0, 0, 3, 0x65, 0xAB, 0xCD]); // one 3-byte IDR NALU

    let mut f = b"FLV\x01\x05".to_vec();
    f.extend_from_slice(&9u32.to_be_bytes()); // DataOffset
    f.extend_from_slice(&0u32.to_be_bytes()); // PreviousTagSize0
    f.extend_from_slice(&tag(18, b"\x02\x00\x0AonMetaData"));
    f.extend_from_slice(&tag(9, &seq_payload));
    f.extend_from_slice(&tag(8, &[0xAF, 0x00, 0x12]));
    f.extend_from_slice(&tag(9, &kf_payload));
    f
}

/// Baseline SPS for 64×48 (hand-packed Exp-Golomb; see `flv.rs`'s tests for the encoder):
/// profile 66, level 30, poc_type 2, 4×3 macroblocks, frame_mbs_only, no cropping. Hoisted to
/// module scope so it can be a fuzz SEED in its own right — `flv::fuzzapi::parse_sps` takes
/// exactly these bytes, and no container-level mutation ever delivers them mutated.
/// `flv_seed_reaches_the_muxer` asserts it stays parseable.
pub(super) const H264_SPS: &[u8] = &[0x67, 0x42, 0x00, 0x1E, 0xDA, 0x11, 0xC4];

/// The AVCDecoderConfigurationRecord wrapping [`H264_SPS`], as an FLV sequence header carries
/// it. Also a seed in its own right, for `flv::fuzzapi::sps_dims`.
pub(super) fn h264_avcc() -> Vec<u8> {
    let mut avcc = vec![1u8, 66, 0, 30, 0xFF, 0xE1];
    avcc.extend_from_slice(&(H264_SPS.len() as u16).to_be_bytes());
    avcc.extend_from_slice(H264_SPS);
    avcc.push(0); // no PPS
    avcc
}

/// A structurally valid FLV whose first video tag is a Flash-codec KEYFRAME (`2` = Sorenson
/// Spark, `4` = VP6) — the shape [`crate::flv::scan_flash_keyframe`] walks.
///
/// [`synthetic_flv`] cannot stand in for this: it is H.264 (codec 7), which that scanner
/// correctly refuses at the first video tag, so it only ever exercises the decline path.
pub(super) fn synthetic_flash_flv(codec_id: u8) -> Vec<u8> {
    fn tag(tag_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut t = vec![tag_type];
        t.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..4]); // u24 DataSize
        t.extend_from_slice(&[0u8; 7]); // timestamp(3) + ext(1) + stream id(3)
        t.extend_from_slice(payload);
        t.extend_from_slice(&((11 + payload.len()) as u32).to_be_bytes());
        t
    }
    // An inter frame FIRST, so the walk has to keep looking rather than stopping on tag one.
    let inter = {
        let mut p = vec![(2u8 << 4) | codec_id]; // FrameType 2 = inter
        p.extend_from_slice(&[0x11; 24]);
        p
    };
    let key = {
        let mut p = vec![(1u8 << 4) | codec_id]; // FrameType 1 = keyframe
                                                 // VP6 keeps a leading adjustment byte the decoder consumes; Sorenson does not.
        if codec_id == 4 {
            p.push(0x00);
        }
        // A plausible Sorenson picture header start (temporal ref + picture start code region)
        // — the point is body bytes for a mutation to land in, not a decodable frame.
        p.extend_from_slice(&[0x00, 0x00, 0x84, 0x00, 0x07, 0x02, 0x87, 0x85]);
        p.extend_from_slice(&[0x5A; 40]);
        p
    };
    let mut f = b"FLV\x01\x05".to_vec();
    f.extend_from_slice(&9u32.to_be_bytes()); // DataOffset
    f.extend_from_slice(&0u32.to_be_bytes()); // PreviousTagSize0
    f.extend_from_slice(&tag(18, b"\x02\x00\x0AonMetaData"));
    f.extend_from_slice(&tag(8, &[0xAF, 0x00, 0x12])); // an audio tag in the way
    f.extend_from_slice(&tag(9, &inter));
    f.extend_from_slice(&tag(9, &key));
    f
}

/// A structurally valid WebM whose first video track is `V_VP9`, with a Cues index pointing at
/// a Cluster that holds a keyframe SimpleBlock — the exact shape [`crate::mkv::vp9_keyframe`]
/// walks (Cues ▸ CueTrackPositions ▸ cluster ▸ SimpleBlock flags ▸ lacing).
///
/// [`synthetic_mkv`] cannot stand in for it: that scaffold declares an HEVC track, and
/// `vp9_keyframe` self-gates on `V_VP9`, so it returns before touching any of the above.
pub(super) fn synthetic_webm_vp9() -> Vec<u8> {
    const ID_EBML: u64 = 0x1A45_DFA3;
    const ID_SEGMENT: u64 = 0x1853_8067;
    const ID_INFO: u64 = 0x1549_A966;
    const ID_TIMECODE_SCALE: u64 = 0x2AD7B1;
    const ID_DURATION: u64 = 0x4489;
    const ID_TRACKS: u64 = 0x1654_AE6B;
    const ID_TRACK_ENTRY: u64 = 0xAE;
    const ID_TRACK_NUMBER: u64 = 0xD7;
    const ID_TRACK_TYPE: u64 = 0x83;
    const ID_CODEC_ID: u64 = 0x86;
    const ID_CUES: u64 = 0x1C53_BB6B;
    const ID_CUE_POINT: u64 = 0xBB;
    const ID_CUE_TIME: u64 = 0xB3;
    const ID_CUE_TRACK_POSITIONS: u64 = 0xB7;
    const ID_CUE_TRACK: u64 = 0xF7;
    const ID_CUE_CLUSTER_POSITION: u64 = 0xF1;
    const ID_CLUSTER: u64 = 0x1F43_B675;
    const ID_CLUSTER_TIMECODE: u64 = 0xE7;
    const ID_SIMPLE_BLOCK: u64 = 0xA3;

    let info = ebml(
        ID_INFO,
        &[
            ebml(ID_TIMECODE_SCALE, &[0x0F, 0x42, 0x40]),
            ebml(ID_DURATION, &1000.0f32.to_be_bytes()),
        ]
        .concat(),
    );
    let tracks = ebml(
        ID_TRACKS,
        &ebml(
            ID_TRACK_ENTRY,
            &[
                ebml(ID_TRACK_NUMBER, &[1]),
                ebml(ID_TRACK_TYPE, &[1]),
                ebml(ID_CODEC_ID, b"V_VP9"),
            ]
            .concat(),
        ),
    );
    // SimpleBlock body: track vint (0x81 = 1), s16 relative timecode, flags (0x80 = keyframe,
    // no lacing), then the frame. The payload starts with a real VP9 uncompressed-header
    // frame marker so the bytes a mutation lands in mean something to the reader downstream.
    let mut block = vec![0x81u8, 0x00, 0x00, 0x80];
    block.extend_from_slice(&[0x82, 0x49, 0x83, 0x42, 0x00, 0x07, 0x00, 0x3C]);
    block.extend_from_slice(&[0xA5; 32]);
    let cluster = ebml(
        ID_CLUSTER,
        &[
            ebml(ID_CLUSTER_TIMECODE, &[0]),
            ebml(ID_SIMPLE_BLOCK, &block),
        ]
        .concat(),
    );
    // CueClusterPosition is written at a FIXED 8-byte width, so building the index twice —
    // once to learn the cluster's offset, once with the real value — cannot shift any length
    // around it. (The same two-pass trick `synthetic_mp4` uses for its stco offsets.)
    let cues_for = |pos: u64| {
        ebml(
            ID_CUES,
            &ebml(
                ID_CUE_POINT,
                &[
                    ebml(ID_CUE_TIME, &[0]),
                    ebml(
                        ID_CUE_TRACK_POSITIONS,
                        &[
                            ebml(ID_CUE_TRACK, &[1]),
                            ebml(ID_CUE_CLUSTER_POSITION, &pos.to_be_bytes()),
                        ]
                        .concat(),
                    ),
                ]
                .concat(),
            ),
        )
    };
    let cluster_rel = (info.len() + tracks.len() + cues_for(0).len()) as u64;
    let body = [info, tracks, cues_for(cluster_rel), cluster].concat();
    let mut file = ebml(ID_EBML, &[0x42, 0x82, 0x84, b'w', b'e', b'b', b'm']);
    file.extend_from_slice(&ebml(ID_SEGMENT, &body));
    file
}

/// A `synthetic_mkv`-shaped Cluster carrying one hand-crafted child element whose SIZE
/// VINT is the EBML "unknown size" reserve (marker byte `0x01` then seven `0xFF`
/// bytes — the largest value an 8-byte size vint can hold, `2^56 - 1`) instead of a
/// real length. Built by hand rather than via [`ebml`]/[`ebml_vint`] because those
/// helpers deliberately AVOID ever emitting this exact reserved pattern.
///
/// This file's own header comment describes the bug class such a declared size finds:
/// a reader that adds it straight to an offset instead of going through
/// `checked_add`. `header_at`/`children` already guard that arithmetic, so this seed
/// doesn't demonstrate a live bug — it LOCKS the guard in, so a future refactor that
/// drops the `checked_add` panics here instead of shipping.
pub(super) fn synthetic_mkv_largesize_bomb() -> Vec<u8> {
    const ID_EBML: u64 = 0x1A45_DFA3;
    const ID_SEGMENT: u64 = 0x1853_8067;
    const ID_CLUSTER: u64 = 0x1F43_B675;
    const ID_SIMPLE_BLOCK: u8 = 0xA3;

    let mut bomb = vec![
        ID_SIMPLE_BLOCK,
        0x01,
        0xFF,
        0xFF,
        0xFF,
        0xFF,
        0xFF,
        0xFF,
        0xFF,
    ];
    bomb.extend_from_slice(&[0xAB; 16]); // a little real data after it, for good measure
    let cluster = ebml(ID_CLUSTER, &bomb);
    let mut file = ebml(ID_EBML, &[0x42, 0x82, 0x84, b'w', b'e', b'b', b'm']);
    file.extend_from_slice(&ebml(ID_SEGMENT, &cluster));
    file
}

/// A minimal 16-bit mono PCM WAV (RIFF/WAVE, `fmt ` then `data`) — the shape
/// `container::waveform::parse_wav` walks. Before this, `audio_art_from_reader`'s only
/// WAV-flavoured input was the 12-byte `RIFF….WAVE` magic stub in [`header_stubs`], so
/// a mutation almost always died at the `fmt `/`data` chunk scan before reaching any
/// of the format/bit-depth arithmetic in `parse_wav`/`sample_to_f32`.
pub(super) fn synthetic_wav() -> Vec<u8> {
    const FRAMES: u32 = 512;
    let mut data = Vec::new();
    for i in 0..FRAMES {
        let s = ((i as i32 % 2000) - 1000) as i16; // small triangle-ish ramp
        data.extend_from_slice(&s.to_le_bytes());
    }
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&44100u32.to_le_bytes());
    w.extend_from_slice(&88200u32.to_le_bytes()); // byte rate
    w.extend_from_slice(&2u16.to_le_bytes()); // block align
    w.extend_from_slice(&16u16.to_le_bytes()); // bits
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data.len() as u32).to_le_bytes());
    w.extend_from_slice(&data);
    w
}

/// A minimal 16-bit mono PCM AIFF (FORM/AIFF, `COMM` then `SSND`) — the
/// big-endian sibling of [`synthetic_wav`], the shape
/// `container::waveform::parse_aiff` walks.
pub(super) fn synthetic_aiff() -> Vec<u8> {
    const FRAMES: u32 = 512;
    let mut samples = Vec::new();
    for i in 0..FRAMES {
        let s = ((i as i32 % 2000) - 1000) as i16;
        samples.extend_from_slice(&s.to_be_bytes());
    }
    let mut comm = Vec::new();
    comm.extend_from_slice(&1u16.to_be_bytes()); // channels
    comm.extend_from_slice(&FRAMES.to_be_bytes()); // numSampleFrames
    comm.extend_from_slice(&16u16.to_be_bytes()); // sampleSize (bits)
    comm.extend_from_slice(&[0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0]); // 80-bit extended rate (unused by the parser)

    let mut ssnd = Vec::new();
    ssnd.extend_from_slice(&0u32.to_be_bytes()); // offset
    ssnd.extend_from_slice(&0u32.to_be_bytes()); // blockSize
    ssnd.extend_from_slice(&samples);

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut c = id.to_vec();
        c.extend_from_slice(&(body.len() as u32).to_be_bytes());
        c.extend_from_slice(body);
        if body.len() % 2 == 1 {
            c.push(0); // chunks are word-aligned, like a real AIFF writer
        }
        c
    }
    let comm_chunk = chunk(b"COMM", &comm);
    let ssnd_chunk = chunk(b"SSND", &ssnd);
    let body_len = 4 /* "AIFF" form type */ + comm_chunk.len() + ssnd_chunk.len();

    let mut f = Vec::new();
    f.extend_from_slice(b"FORM");
    f.extend_from_slice(&(body_len as u32).to_be_bytes());
    f.extend_from_slice(b"AIFF");
    f.extend_from_slice(&comm_chunk);
    f.extend_from_slice(&ssnd_chunk);
    f
}

/// A minimal ASF/WMA header carrying one `WM/Picture` attribute in the Extended
/// Content Description Object — the shape `container::audio::asf::asf_cover` walks
/// (GUID-tagged objects, `ecd_attrs`'s name/type/value descriptor stream, then
/// `parse_wm_picture`'s own length-prefixed MIME/description/image fields). Before
/// this, the only ASF-flavoured input was the 4-byte GUID-prefix magic stub in
/// [`header_stubs`], so a mutation never reached any of that arithmetic.
///
/// GUID bytes and the descriptor layouts are copied from `container/audio/asf.rs`
/// (read, not imported — those constants are `pub(super)` to that module and this
/// file cannot see them); they must stay byte-for-byte in sync with that file's own
/// on-disk format or this seed stops reaching the parser.
pub(super) fn synthetic_asf() -> Vec<u8> {
    const ASF_HEADER_GUID: [u8; 16] = [
        0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE,
        0x6C,
    ];
    const ASF_ECD_GUID: [u8; 16] = [
        0x40, 0xA4, 0xD0, 0xD2, 0x07, 0xE3, 0xD2, 0x11, 0x97, 0xF0, 0x00, 0xA0, 0xC9, 0x5E, 0xA8,
        0x50,
    ];
    // Just enough bytes to pass `looks_like_raster` (JPEG SOI + APP0 marker).
    const FAKE_JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F'];

    fn utf16z(s: &str) -> Vec<u8> {
        let mut v: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
        v.extend_from_slice(&[0, 0]); // NUL terminator
        v
    }

    let mut wm_picture = vec![3u8]; // picture type 3 = front cover
    wm_picture.extend_from_slice(&(FAKE_JPEG.len() as u32).to_le_bytes());
    wm_picture.extend_from_slice(&utf16z("image/jpeg"));
    wm_picture.extend_from_slice(&utf16z(""));
    wm_picture.extend_from_slice(FAKE_JPEG);

    let name = utf16z("WM/Picture");
    let mut ecd_payload = 1u16.to_le_bytes().to_vec(); // descriptor count
    ecd_payload.extend_from_slice(&(name.len() as u16).to_le_bytes());
    ecd_payload.extend_from_slice(&name);
    ecd_payload.extend_from_slice(&1u16.to_le_bytes()); // value type 1 = byte array
    ecd_payload.extend_from_slice(&(wm_picture.len() as u16).to_le_bytes());
    ecd_payload.extend_from_slice(&wm_picture);

    let mut ecd = ASF_ECD_GUID.to_vec();
    ecd.extend_from_slice(&((24 + ecd_payload.len()) as u64).to_le_bytes());
    ecd.extend_from_slice(&ecd_payload);

    let mut header = ASF_HEADER_GUID.to_vec();
    header.extend_from_slice(&((30 + ecd.len()) as u64).to_le_bytes()); // header object size
    header.extend_from_slice(&1u32.to_le_bytes()); // number of header sub-objects
    header.extend_from_slice(&[1, 2]); // reserved
    header.extend_from_slice(&ecd);
    header
}

pub(super) fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// See `header_stubs`: PNG signature, a 1x1 16-bit RGB IHDR, a cICP chunk, an empty IDAT
/// and IEND, with dummy CRCs.
pub(super) fn png_cicp_stub() -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    let chunk = |typ: &[u8; 4], data: &[u8]| {
        let mut c = (data.len() as u32).to_be_bytes().to_vec();
        c.extend_from_slice(typ);
        c.extend_from_slice(data);
        c.extend_from_slice(&[0, 0, 0, 0]);
        c
    };
    v.extend(chunk(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 16, 2, 0, 0, 0]));
    v.extend(chunk(b"cICP", &[9, 16, 0, 1]));
    v.extend(chunk(b"IDAT", &[]));
    v.extend(chunk(b"IEND", &[]));
    v
}

/// Tiny format headers — enough magic to send each sniffer/extractor down its real path.
pub(super) fn header_stubs() -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = vec![
        b"PK\x03\x04".to_vec(),              // zip local file header
        b"7z\xBC\xAF\x27\x1C".to_vec(),      // 7z
        b"Rar!\x1A\x07\x00".to_vec(),        // rar4
        b"Rar!\x1A\x07\x01\x00".to_vec(),    // rar5
        b"8BPS\0\x01".to_vec(),              // psd
        b"FORM\0\0\0\x10ILBM".to_vec(),      // iff/ilbm
        b"DDS \x7C\0\0\0".to_vec(),          // dds
        b"%!PS-Adobe-3.0 EPSF-3.0".to_vec(), // eps
        b"OggS\0\x02".to_vec(),              // ogg
        b"RIFF\0\0\0\0WEBP".to_vec(),        // webp/riff
        b"RIFF\0\0\0\0AVI ".to_vec(),        // avi
        b"ID3\x03\0".to_vec(),               // mp3/id3
        b"fLaC\0\0\0\x22".to_vec(),          // flac
        b"BLENDER-v300".to_vec(),            // blend
        b"\x89PNG\r\n\x1a\n".to_vec(),       // png sig
    ];
    // A PNG header with a `cICP` chunk (BT.2020 / PQ / full range) ahead of a stub IDAT,
    // so the chunk walk is mutated past its signature check (CRCs are not checked by it).
    v.push(png_cicp_stub());
    // MP4 audio brand (M4A) — routes the ISO-BMFF path as audio, not video.
    v.push(mp4box(b"ftyp", b"M4A \0\0\0\0M4A mp42isom"));
    v
}
