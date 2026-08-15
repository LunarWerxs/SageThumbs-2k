//! FLV (Flash Video) → one-keyframe mini-MP4, for H.264 content only.
//!
//! Windows Media Foundation has no FLV demuxer, so an `.flv` never even opens through the
//! normal video tiers — but the H.264 *codec* inside most FLVs is the same inbox decoder
//! every MP4 uses. The bridge is small because FLV stores AVC exactly the way MP4 does:
//! the "AVC sequence header" video tag's payload IS an `AVCDecoderConfigurationRecord`
//! (byte-identical to an `avcC` box body), and each NALU video tag's payload is already
//! length-prefixed AVCC sample data. So: walk the tags, take the config + the first
//! keyframe, wrap the config in an `avc1` sample entry, and hand both to the pure muxer
//! [`crate::mp4::build_mini_mp4`] — the result decodes through
//! [`crate::video::frame_from_bytes`] like any other mini-MP4.
//!
//! Codec ids other than 7 (AVC) — Sorenson Spark (2), VP6 (4/5) — have no Windows decoder
//! and no viable pure-Rust one, so they return `None` cleanly (default icon, same as today).
//!
//! This parses UNTRUSTED bytes in-process (`panic = "abort"` inside explorer.exe), hence:
//! no indexing, no unwraps, checked arithmetic on every file-supplied length, a strictly
//! forward tag walk with tag-count and byte caps, and capped allocations throughout.

use std::io::{Read, Seek, SeekFrom};

use crate::mp4::{build_mini_mp4, bx, fbx, read_exact_at};

/// Give up after this many tags. A real pre-keyframe run is a metadata tag plus a handful
/// of audio tags; hundreds of thousands means a crafted file, not a video.
const MAX_TAGS: u32 = 200_000;
/// Never walk past this absolute offset looking for the config + first keyframe. Both live
/// near the head of any real FLV (the sequence header is the first video tag by spec).
const WALK_MAX: u64 = 256 * 1024 * 1024;
/// Sanity cap on the AVCDecoderConfigurationRecord (typically well under 100 bytes).
const CONFIG_MAX: usize = 64 * 1024;
/// Sanity cap on the keyframe sample. An FLV tag's DataSize field is 24-bit so the format
/// can't exceed 16 MiB anyway; this is belt-and-braces against a parsing slip.
const KEYFRAME_MAX: usize = 16 * 1024 * 1024;
/// Largest plausible FLV header DataOffset (spec value is 9; some writers pad slightly).
const HEADER_MAX: u64 = 4096;

/// Build a one-keyframe mini-MP4 from an FLV whose video codec is H.264. Returns the
/// mini-MP4 bytes for [`crate::video::frame_from_bytes`], or `None` for non-FLV input,
/// non-AVC codecs, or any malformed/truncated structure (caller falls through).
///
/// Unlike the MP4/MKV twins this takes no time fraction: FLV has no sample index to seek
/// (`onMetaData` keyframe tables are optional and untrustworthy), so the representative
/// frame is the FIRST keyframe after the sequence header — a targeted read of the file
/// head, never a whole-file scan.
pub fn keyframe_mini_mp4<R: Read + Seek>(r: &mut R) -> Option<Vec<u8>> {
    let total = r.seek(SeekFrom::End(0)).ok()?;
    // Header (9) + PreviousTagSize0 (4) + one tag header (11) is the smallest useful file.
    if total < 24 {
        return None;
    }
    let mut hdr = [0u8; 9];
    read_exact_at(r, 0, &mut hdr)?;
    if &hdr[0..3] != b"FLV" {
        return None;
    }
    let data_offset = u32::from_be_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]) as u64;
    if !(9..=HEADER_MAX).contains(&data_offset) {
        return None;
    }

    let mut avc_config: Option<Vec<u8>> = None;
    let mut pos = data_offset.checked_add(4)?; // skip PreviousTagSize0
    let mut tags = 0u32;
    // Tag layout: type(1) DataSize(3) Timestamp(3) TimestampExt(1) StreamID(3) then
    // Data[DataSize] then PreviousTagSize(4). The advance is 11 + DataSize + 4 ≥ 15, so the
    // walk is STRUCTURALLY forward-only — a hostile DataSize can overshoot (caught by the
    // bounds check) but never stall or rewind. The caps bound a long crafted crawl anyway.
    while pos.checked_add(11)? <= total {
        tags = tags.checked_add(1)?;
        if tags > MAX_TAGS || pos > WALK_MAX {
            return None;
        }
        let mut th = [0u8; 11];
        read_exact_at(r, pos, &mut th)?;
        let tag_type = th[0] & 0x1F; // top bits: reserved + encryption filter
        let data_size = u32::from_be_bytes([0, th[1], th[2], th[3]]) as u64;
        let payload_pos = pos.checked_add(11)?;
        if payload_pos.checked_add(data_size)? > total {
            return None; // truncated mid-tag
        }
        // VideoData: FrameType(4 bits) CodecID(4 bits), then for AVC:
        // AVCPacketType(1) CompositionTime(3), then the body.
        if tag_type == 9 && data_size >= 2 {
            let mut vh = [0u8; 2];
            read_exact_at(r, payload_pos, &mut vh)?;
            let frame_type = vh[0] >> 4;
            let codec_id = vh[0] & 0x0F;
            if codec_id != 7 {
                // Sorenson Spark / VP6 / anything else: no decoder available — stop cleanly.
                return None;
            }
            let packet_type = vh[1];
            if data_size >= 5 {
                let body_len = (data_size - 5) as usize;
                if packet_type == 0 && avc_config.is_none() && body_len > 0 {
                    // AVC sequence header: the payload is an AVCDecoderConfigurationRecord.
                    if body_len > CONFIG_MAX {
                        return None;
                    }
                    let mut cfg = vec![0u8; body_len];
                    read_exact_at(r, payload_pos.checked_add(5)?, &mut cfg)?;
                    if cfg.first() != Some(&1) || cfg.len() < 7 {
                        return None; // configurationVersion must be 1
                    }
                    avc_config = Some(cfg);
                } else if packet_type == 1 && frame_type == 1 && body_len > 0 {
                    // A keyframe NALU tag — usable once the config has been seen.
                    if let Some(cfg) = &avc_config {
                        if body_len > KEYFRAME_MAX {
                            return None;
                        }
                        let mut keyframe = vec![0u8; body_len];
                        read_exact_at(r, payload_pos.checked_add(5)?, &mut keyframe)?;
                        return mux(cfg, &keyframe);
                    }
                }
            }
        }
        pos = payload_pos.checked_add(data_size)?.checked_add(4)?;
    }
    None
}

/// Wrap the AVC config + keyframe sample in a mini-MP4. The `stsd` is synthesized (an
/// `avc1` visual sample entry carrying the config verbatim as its `avcC` box); dimensions
/// come from the SPS inside the config. FLV timestamps are milliseconds, so timescale 1000
/// with a nominal 40 ms (25 fps) frame duration — the value only shapes the mvhd/stts of a
/// one-frame movie, it does not affect decoding.
fn mux(avc_config: &[u8], keyframe: &[u8]) -> Option<Vec<u8>> {
    let (width, height) = sps_dims(avc_config)?;
    let stsd = build_stsd(avc_config, width, height);
    Some(build_mini_mp4(
        None, &stsd, 1, 40, 1000, width, height, keyframe,
    ))
}

/// An MP4 `stsd` full box holding one `avc1` VisualSampleEntry whose `avcC` payload is the
/// FLV's AVCDecoderConfigurationRecord, verbatim.
fn build_stsd(avc_config: &[u8], width: u16, height: u16) -> Vec<u8> {
    let avcc = bx(b"avcC", avc_config);
    // VisualSampleEntry (ISO 14496-12 §12.1.3): reserved(6) data_ref_index(2) pre_defined(2)
    // reserved(2) pre_defined(12) width(2) height(2) hres(4) vres(4) reserved(4)
    // frame_count(2) compressorname(32) depth(2) pre_defined(2), then the codec box.
    let mut vse = Vec::with_capacity(78 + avcc.len());
    vse.extend_from_slice(&[0u8; 6]);
    vse.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    vse.extend_from_slice(&[0u8; 16]); // pre_defined + reserved + pre_defined[3]
    vse.extend_from_slice(&width.to_be_bytes());
    vse.extend_from_slice(&height.to_be_bytes());
    vse.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // 72 dpi horizontal
    vse.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // 72 dpi vertical
    vse.extend_from_slice(&[0u8; 4]); // reserved
    vse.extend_from_slice(&1u16.to_be_bytes()); // frame_count
    vse.extend_from_slice(&[0u8; 32]); // compressorname
    vse.extend_from_slice(&24u16.to_be_bytes()); // depth
    vse.extend_from_slice(&0xFFFFu16.to_be_bytes()); // pre_defined = -1
    vse.extend_from_slice(&avcc);
    let avc1 = bx(b"avc1", &vse);
    let mut body = 1u32.to_be_bytes().to_vec(); // entry_count
    body.extend_from_slice(&avc1);
    fbx(b"stsd", 0, 0, &body)
}

// ---------------------------------------------------------------------------------------------
// SPS geometry
// ---------------------------------------------------------------------------------------------

/// Coded width/height from the first SPS in an AVCDecoderConfigurationRecord.
/// Record layout: version(1) profile(1) compat(1) level(1) 0b111111·lengthSizeMinusOne(1)
/// 0b111·numSPS(1), then per SPS: length(2) + the NAL (header byte + RBSP with emulation-
/// prevention bytes).
fn sps_dims(cfg: &[u8]) -> Option<(u16, u16)> {
    if cfg.first() != Some(&1) {
        return None;
    }
    let num_sps = cfg.get(5)? & 0x1F;
    if num_sps == 0 {
        return None;
    }
    let len = u16::from_be_bytes([*cfg.get(6)?, *cfg.get(7)?]) as usize;
    let sps = cfg.get(8..8usize.checked_add(len)?)?;
    let (nal_header, payload) = sps.split_first()?;
    if nal_header & 0x1F != 7 {
        return None; // not a seq_parameter_set NAL
    }
    let rbsp = strip_emulation_prevention(payload);
    parse_sps(&rbsp)
}

/// Remove H.264 emulation-prevention bytes: any 0x03 that follows two 0x00s is an escape,
/// not data.
fn strip_emulation_prevention(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut zeros = 0u32;
    for &x in b {
        if zeros >= 2 && x == 3 {
            zeros = 0;
            continue;
        }
        zeros = if x == 0 { zeros + 1 } else { 0 };
        out.push(x);
    }
    out
}

/// MSB-first bit reader over an RBSP slice. Every read is bounds-checked (`None` past the
/// end), so a truncated SPS aborts the parse instead of reading garbage.
struct Bits<'a> {
    d: &'a [u8],
    pos: usize, // in bits
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.d.get(self.pos / 8)?;
        let b = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos = self.pos.checked_add(1)?;
        Some(u32::from(b))
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n.min(32) {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    /// Exp-Golomb unsigned: count leading zeros, then read that many bits.
    fn ue(&mut self) -> Option<u32> {
        let mut leading = 0u32;
        while self.bit()? == 0 {
            leading += 1;
            if leading > 31 {
                return None; // no valid SPS field needs codes this long
            }
        }
        let rest = self.bits(leading)?;
        1u32.checked_shl(leading)?.checked_sub(1)?.checked_add(rest)
    }

    /// Exp-Golomb signed, in i64 so the mapping can't overflow on hostile input.
    fn se(&mut self) -> Option<i64> {
        let k = i64::from(self.ue()?);
        Some(if k % 2 == 0 { -(k / 2) } else { (k + 1) / 2 })
    }
}

/// Skip one scaling list (ITU-T H.264 §7.3.2.1.1.1) — the values are irrelevant here, but
/// the delta run must be consumed exactly for everything after it to line up.
fn skip_scaling_list(b: &mut Bits, size: u32) -> Option<()> {
    let mut last: i64 = 8;
    let mut next: i64 = 8;
    for _ in 0..size {
        if next != 0 {
            let delta = b.se()?;
            next = (last + delta).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// Frame geometry from an SPS RBSP (ITU-T H.264 §7.3.2.1.1): walk every field ahead of
/// `pic_width_in_mbs_minus1`, apply the frame-cropping rectangle in chroma-scaled units.
fn parse_sps(rbsp: &[u8]) -> Option<(u16, u16)> {
    let mut b = Bits { d: rbsp, pos: 0 };
    let profile_idc = b.bits(8)?;
    b.bits(8)?; // constraint_set flags + reserved
    b.bits(8)?; // level_idc
    b.ue()?; // seq_parameter_set_id

    let mut chroma_format_idc = 1u32; // 4:2:0 unless the profile carries it explicitly
    let mut separate_colour_planes = false;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = b.ue()?;
        if chroma_format_idc > 3 {
            return None;
        }
        if chroma_format_idc == 3 {
            separate_colour_planes = b.bit()? == 1;
        }
        b.ue()?; // bit_depth_luma_minus8
        b.ue()?; // bit_depth_chroma_minus8
        b.bit()?; // qpprime_y_zero_transform_bypass_flag
        if b.bit()? == 1 {
            // seq_scaling_matrix_present
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if b.bit()? == 1 {
                    skip_scaling_list(&mut b, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }

    b.ue()?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = b.ue()?;
    if pic_order_cnt_type == 0 {
        b.ue()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if pic_order_cnt_type == 1 {
        b.bit()?; // delta_pic_order_always_zero_flag
        b.se()?; // offset_for_non_ref_pic
        b.se()?; // offset_for_top_to_bottom_field
        let n = b.ue()?;
        if n > 255 {
            return None; // spec caps num_ref_frames_in_pic_order_cnt_cycle at 255
        }
        for _ in 0..n {
            b.se()?;
        }
    } else if pic_order_cnt_type > 2 {
        return None;
    }
    b.ue()?; // max_num_ref_frames
    b.bit()?; // gaps_in_frame_num_value_allowed_flag

    let pic_width_in_mbs_minus1 = b.ue()?;
    let pic_height_in_map_units_minus1 = b.ue()?;
    let frame_mbs_only = b.bit()?;
    if frame_mbs_only == 0 {
        b.bit()?; // mb_adaptive_frame_field_flag
    }
    b.bit()?; // direct_8x8_inference_flag

    let (mut crop_l, mut crop_r, mut crop_t, mut crop_b) = (0u32, 0u32, 0u32, 0u32);
    if b.bit()? == 1 {
        crop_l = b.ue()?;
        crop_r = b.ue()?;
        crop_t = b.ue()?;
        crop_b = b.ue()?;
    }

    let width_px = pic_width_in_mbs_minus1.checked_add(1)?.checked_mul(16)?;
    let height_px = pic_height_in_map_units_minus1
        .checked_add(1)?
        .checked_mul(16)?
        .checked_mul(2u32.checked_sub(frame_mbs_only)?)?;

    // Crop units scale with the chroma sampling (§7.4.2.1.1): SubWidthC/SubHeightC for
    // chroma 1/2, unity for monochrome, 4:4:4, and separate colour planes.
    let chroma_array_type = if separate_colour_planes {
        0
    } else {
        chroma_format_idc
    };
    let (unit_x, unit_y_base) = match chroma_array_type {
        1 => (2u32, 2u32), // 4:2:0
        2 => (2, 1),       // 4:2:2
        _ => (1, 1),       // mono / 4:4:4 / separate planes
    };
    let unit_y = unit_y_base.checked_mul(2u32.checked_sub(frame_mbs_only)?)?;

    let w = width_px.checked_sub(crop_l.checked_add(crop_r)?.checked_mul(unit_x)?)?;
    let h = height_px.checked_sub(crop_t.checked_add(crop_b)?.checked_mul(unit_y)?)?;
    if !(1..=16384).contains(&w) || !(1..=16384).contains(&h) {
        return None;
    }
    Some((w as u16, h as u16))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::Path;

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

    /// The corpus `sample.flv` is Sorenson Spark (codec id 2) — the correct answer for it
    /// is a clean decline, exactly what the pre-FLV-support behaviour was. Skips when the
    /// dev corpus isn't present (CI).
    #[test]
    fn corpus_sorenson_flv_declines_cleanly() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("test-corpus")
            .join("sample.flv");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("corpus_sorenson_flv: no sample.flv — skipping");
            return;
        };
        assert_eq!(keyframe_mini_mp4(&mut Cursor::new(&bytes)), None);
    }

    /// End-to-end through Media Foundation: take the REAL avcC + keyframe out of the corpus
    /// `sample.mp4`'s mini-MP4 (H.264), wrap them in a synthetic FLV, run the FLV path, and
    /// decode the result. Proves the FLV → mini-MP4 → MF chain on genuine H.264 bytes
    /// without committing a video fixture. Skips when no corpus sample is available.
    #[test]
    fn real_h264_flv_round_trips_through_mediafoundation() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("test-corpus")
            .join("sample.mp4");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("real_h264_flv_round_trips: no sample.mp4 — skipping");
            return;
        };
        let mini = crate::mp4::keyframe_mini_mp4(&mut Cursor::new(&bytes), 0.30)
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
        let mini2 = keyframe_mini_mp4(&mut Cursor::new(&flv))
            .expect("mini-mp4 from the synthesized H.264 FLV");
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
}
