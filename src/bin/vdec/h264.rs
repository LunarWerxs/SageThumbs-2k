//! The `st2k h264-frame` decode core: an H.264 decoder configuration record plus one keyframe
//! → PNG. The CHILD side of `st2k_codecs::h264::h264_frame` (see [`super`] for the shared
//! containment story; issue #52 for why it exists).
//!
//! 1. Splits the parent's framing (`h264::split_child_input`) and reads every SPS the decoder
//!    will see BEFORE decoding, the configuration record's and any the keyframe repeats in
//!    band: each one's coded size is refused against `decode::limits::MAX_DIM` before the
//!    decoder can allocate a picture for it, and the VUI of the one the slices name
//!    (`video_full_range_flag`, `matrix_coefficients`) drives the colour conversion.
//! 2. Decodes with the vendored `rust_h264` (`crates/vendor/rust_h264`, with the high bit depth
//!    patch in `crates/vendor/rust_h264-patches`): the record's SPS and PPS, then the
//!    keyframe's NAL units, then a flush for the picture. Before it shipped, 20k mutations of
//!    the High 10 and 8-bit fixtures' framed input went through [`frame_png`]: no panic, no
//!    hang, the slowest 34 ms (2026-10-01).
//! 3. Converts to 8-bit RGBA the way the VP9 child does: samples normalised from their native
//!    depth (luma and chroma depths may differ), studio swing expanded unless the stream says
//!    full range, and the matrix the stream names (BT.709, BT.601, SMPTE 240M, BT.2020), with
//!    "unspecified" resolved by frame size like every player (≥1280×720 → BT.709).
//! 4. Stretches the width to the sample aspect ratio the SPS gives, so an anamorphic DVD-sized
//!    encode (720x480 at 8:9) comes out at the 4:3 it is shown at.

use std::collections::HashMap;

use rust_h264::decoder::{Frame, OrderedDecoder};
use rust_h264::nal::{parse_avcc, parse_avcc_config, NalUnit, NalUnitType};
use rust_h264::sps::{parse_sps, Sps};
use st2k_codecs::h264::{split_child_input, MAX_DIM};

/// The testable core: the parent's framed input → PNG bytes.
pub(super) fn frame_png(input: &[u8]) -> Result<Vec<u8>, String> {
    let (config, keyframe) = split_child_input(input).ok_or("malformed child input")?;
    let avcc = parse_avcc_config(config).map_err(|e| format!("avcC: {e}"))?;
    let units = parse_avcc(keyframe, avcc.length_size);
    let sps = active_sps(avcc.sps_nals.iter().chain(&avcc.pps_nals).chain(&units))?;
    let picture = decode(&avcc.sps_nals, &avcc.pps_nals, &units)?;
    let rgba = to_rgba(&picture, &sps)?;
    let img = image::RgbaImage::from_raw(picture.width, picture.height, rgba)
        .ok_or("RGBA size does not match the frame")?;
    // Stretched to the shape it is SHOWN at (the reporter's 720x480 file is 4:3, not 3:2).
    let (sar_w, sar_h) = sps.sample_aspect_ratio;
    let shown = st2k_codecs::video::apply_pixel_aspect(
        image::DynamicImage::ImageRgba8(img),
        (u32::from(sar_w), u32::from(sar_h)),
    );
    super::encode_png(shown.width(), shown.height(), shown.into_rgba8().into_raw())
}

/// Walks the NAL units in the order the decoder is fed them (the record's SPS and PPS, then the
/// keyframe's) and returns the SPS the first slice is decoded with: its VUI is the one that
/// describes the picture.
///
/// Every SPS on the way, the record's AND any the keyframe repeats in band, is refused against
/// `MAX_DIM` by its CODED size (whole macroblocks, before the crop) — the size the decoder
/// allocates — so no parameter set reaches it unchecked. One `parse_sps` refuses, the decoder
/// refuses too (it parses with the same function), so it cannot size anything.
///
/// The first slice names a PPS, the PPS names an SPS, and a later set with the same id
/// replaces an earlier one, as in the decoder. A chain that does not resolve falls back to the
/// last SPS seen: the decoder cannot decode that slice either.
fn active_sps<'a, 'b: 'a>(nals: impl Iterator<Item = &'a NalUnit<'b>>) -> Result<Sps, String> {
    let mut sps_by_id: HashMap<u32, Sps> = HashMap::new();
    let mut pps_to_sps: HashMap<u32, u32> = HashMap::new();
    let (mut active, mut last_id) = (None, None);
    for nal in nals {
        match nal.nal_unit_type {
            NalUnitType::Pps => {
                if let Some([pps_id, sps_id]) = leading_ue(&nal.rbsp) {
                    pps_to_sps.insert(pps_id, sps_id);
                }
                continue;
            }
            NalUnitType::Slice | NalUnitType::SliceIdr if active.is_none() => {
                // slice_header: first_mb_in_slice, slice_type, pic_parameter_set_id.
                let referenced = leading_ue::<3>(&nal.rbsp)
                    .and_then(|[_, _, pps_id]| pps_to_sps.get(&pps_id).copied());
                active = referenced.and_then(|id| sps_by_id.remove(&id));
                continue;
            }
            NalUnitType::Sps => {}
            _ => continue,
        }
        let Ok(sps) = parse_sps(&nal.rbsp) else {
            continue;
        };
        let coded_w = sps
            .pic_width_in_mbs_minus1
            .saturating_add(1)
            .saturating_mul(16);
        let coded_h = sps
            .pic_height_in_map_units_minus1
            .saturating_add(1)
            .saturating_mul(if sps.frame_mbs_only_flag { 16 } else { 32 });
        if sps.width() == 0 || sps.height() == 0 || coded_w > MAX_DIM || coded_h > MAX_DIM {
            return Err(format!(
                "refusing a {coded_w}x{coded_h} coded frame (cap {MAX_DIM})"
            ));
        }
        last_id = Some(sps.seq_parameter_set_id);
        sps_by_id.insert(sps.seq_parameter_set_id, sps);
    }
    active
        .or_else(|| last_id.and_then(|id| sps_by_id.remove(&id)))
        .ok_or_else(|| "no readable SPS in the record or the keyframe".to_string())
}

/// The first `N` unsigned Exp-Golomb values of an RBSP (ITU-T H.264 §9.1), or `None` if it
/// ends first. Enough to follow a slice to its PPS and a PPS to its SPS, both of which open
/// with these ids.
fn leading_ue<const N: usize>(rbsp: &[u8]) -> Option<[u32; N]> {
    let bit = |pos: usize| {
        rbsp.get(pos / 8)
            .map(|b| u32::from((b >> (7 - pos % 8)) & 1))
    };
    let mut pos = 0;
    let mut out = [0u32; N];
    for slot in &mut out {
        let mut zeros = 0;
        while bit(pos)? == 0 {
            zeros += 1;
            pos += 1;
            if zeros > 31 {
                return None;
            }
        }
        pos += 1;
        let mut v = 1u32;
        for _ in 0..zeros {
            v = (v << 1) | bit(pos)?;
            pos += 1;
        }
        *slot = v - 1;
    }
    Some(out)
}

/// Feed the parameter sets, then the keyframe's NAL units, and return the first picture.
///
/// An error on one NAL unit does not end the decode: a keyframe access unit can carry SEI,
/// AUD or filler units that matter to nobody here. Only "no picture came out" is a failure,
/// reported with the last error seen, which is the one that explains it.
fn decode(
    sps: &[NalUnit<'_>],
    pps: &[NalUnit<'_>],
    units: &[NalUnit<'_>],
) -> Result<Frame, String> {
    let mut decoder = OrderedDecoder::new();
    let mut frames = Vec::new();
    let mut last_error = None;
    for nal in sps.iter().chain(pps).chain(units) {
        match decoder.decode_nal(nal) {
            Ok(out) => frames.extend(out),
            Err(e) => last_error = Some(e.to_string()),
        }
    }
    frames.extend(decoder.flush());
    frames.into_iter().next().ok_or_else(|| {
        last_error.map_or_else(
            || "the keyframe held no picture".to_string(),
            |e| format!("decode: {e}"),
        )
    })
}

/// The YUV→RGB matrix (Kr, Kb) the SPS names, by ITU-T H.273 `matrix_coefficients`.
fn matrix(sps: &Sps, w: usize, h: usize) -> (f32, f32) {
    match sps.matrix_coefficients {
        1 => (0.2126, 0.0722),
        4..=6 => (0.299, 0.114),
        7 => (0.212, 0.087),
        9 | 10 => (0.2627, 0.0593),
        // Unspecified (2) and the rest resolve by frame size, as every player does.
        _ if w >= 1280 || h >= 720 => (0.2126, 0.0722),
        _ => (0.299, 0.114),
    }
}

/// Convert a decoded 4:2:0 picture to 8-bit RGBA (see the module docs for the exact rules).
fn to_rgba(f: &Frame, sps: &Sps) -> Result<Vec<u8>, String> {
    let (w, h, cw) = plane_geometry(f)?;
    let (kr, kb) = matrix(sps, w, h);
    let kg = 1.0 - kr - kb;
    let (by, bc) = (u32::from(f.bit_depth_luma), u32::from(f.bit_depth_chroma));
    // Level normalisation, generic over bit depth: studio swing spans 16..235 for luma and
    // 16..240 for chroma scaled by 2^(bd-8); full range spans 0..2^bd-1. Chroma is centred
    // at 2^(bd-1) either way.
    let (sy, sc) = ((1u32 << (by - 8)) as f32, (1u32 << (bc - 8)) as f32);
    let (y_lo, y_range, c_range) = if sps.video_full_range_flag {
        (0.0, ((1u32 << by) - 1) as f32, ((1u32 << bc) - 1) as f32)
    } else {
        (16.0 * sy, 219.0 * sy, 224.0 * sc)
    };
    let c_mid = (1u32 << (bc - 1)) as f32;

    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        let crow = (row / 2) * cw;
        for col in 0..w {
            let yy = ((f32::from(f.y[row * w + col]) - y_lo) / y_range).clamp(0.0, 1.0);
            let ci = crow + col / 2;
            let cb = (f32::from(f.u[ci]) - c_mid) / c_range;
            let cr = (f32::from(f.v[ci]) - c_mid) / c_range;
            let r = yy + 2.0 * (1.0 - kr) * cr;
            let b = yy + 2.0 * (1.0 - kb) * cb;
            let g = (yy - kr * r - kb * b) / kg;
            super::push_rgb(&mut rgba, r, g, b);
        }
    }
    Ok(rgba)
}

/// Re-check what the decoder reports and return the validated layout `(w, h, chroma width)`;
/// the plane sizes are refuted before any RGBA allocation (an inconsistency would otherwise
/// panic on an index below, and this whole process is panic=abort).
fn plane_geometry(f: &Frame) -> Result<(usize, usize, usize), String> {
    if f.width == 0 || f.height == 0 || f.width > MAX_DIM || f.height > MAX_DIM {
        return Err(format!(
            "decoder returned a {}x{} frame (cap {MAX_DIM})",
            f.width, f.height
        ));
    }
    if !(8..=14).contains(&f.bit_depth_luma) || !(8..=14).contains(&f.bit_depth_chroma) {
        return Err("implausible bit depth from the decoder".into());
    }
    let (w, h) = (f.width as usize, f.height as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    if f.y.len() != w * h || f.u.len() != cw * ch || f.v.len() != cw * ch {
        return Err("decoded plane sizes are inconsistent".into());
    }
    Ok((w, h, cw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Issue #52's shape, end to end through this process's own pieces: a High 10 (10-bit
    /// 4:2:0) H.264 in Matroska, one flat colour, anamorphic like the reporter's DVD-sized
    /// encode, cut by the parent's reader into the child's framing and decoded here. ffmpeg
    /// wrote it from a 72x48 `color=#3366cc` with `setsar=8/9` through `-profile:v high10
    /// -pix_fmt yuv420p10le` (BT.601 studio swing, no VUI matrix), so the picture must come
    /// back as that colour, give or take the codec, at the 64x48 it is shown at.
    #[test]
    fn a_high10_matroska_keyframe_decodes_to_its_colour() {
        let png = frame_png(&high10_input(None)).expect("High 10 decodes");
        assert_is_3366cc_at_64x48(&png);
    }

    /// The fixture's child input, with `inband` (one NAL unit) placed ahead of the keyframe's
    /// own units, where a stream that repeats its parameter sets carries them.
    fn high10_input(inband: Option<&[u8]>) -> Vec<u8> {
        let mkv = include_bytes!("../../../tests/fixtures/h264/high10-3366cc.mkv");
        let input = st2k_codecs::h264::child_input(&mut Cursor::new(&mkv[..]), 0.30)
            .expect("the reader finds the H.264 track's record and keyframe");
        let Some(nal) = inband else {
            return input;
        };
        let cfg_len = u32::from_le_bytes(input[..4].try_into().unwrap()) as usize;
        let length_size = usize::from(input[4 + 4] & 3) + 1;
        let mut spliced = input[..4 + cfg_len].to_vec();
        spliced.extend_from_slice(&(nal.len() as u32).to_be_bytes()[4 - length_size..]);
        spliced.extend_from_slice(nal);
        spliced.extend_from_slice(&input[4 + cfg_len..]);
        spliced
    }

    fn assert_is_3366cc_at_64x48(png: &[u8]) {
        let img = image::load_from_memory(png).expect("PNG").to_rgba8();
        assert_eq!((img.width(), img.height()), (64, 48));
        let px = img.get_pixel(32, 24).0;
        for (got, want) in px.iter().zip([0x33u8, 0x66, 0xcc]) {
            assert!(
                got.abs_diff(want) <= 3,
                "centre pixel {px:?}, want about (51, 102, 204)"
            );
        }
    }

    /// A Baseline SPS NAL unit: `fields` are the bits after `profile_idc`, `constraint_flags`
    /// and `level_idc`, up to and including the RBSP stop bit.
    fn baseline_sps(fields: &[bool]) -> Vec<u8> {
        let mut rbsp = vec![66u8, 0, 30];
        rbsp.extend(fields.chunks(8).map(|c| {
            c.iter()
                .enumerate()
                .fold(0u8, |b, (i, &on)| b | (u8::from(on) << (7 - i)))
        }));
        let mut nal = vec![0x67u8];
        for &b in &rbsp {
            if b <= 3 && nal.ends_with(&[0, 0]) {
                nal.push(3); // emulation prevention
            }
            nal.push(b);
        }
        nal
    }

    /// Appends `v` as unsigned Exp-Golomb.
    fn ue(bits: &mut Vec<bool>, v: u32) {
        let x = v + 1;
        let n = 32 - x.leading_zeros();
        bits.extend(std::iter::repeat_n(false, n as usize - 1));
        bits.extend((0..n).rev().map(|i| (x >> i) & 1 == 1));
    }

    /// The SPS fields up to `frame_mbs_only_flag`: `id`, log2_max_frame_num-4 0, POC type 2,
    /// one reference frame, no gaps, then the size in macroblocks.
    fn sps_head(id: u32, width_mbs: u32, height_mbs: u32) -> Vec<bool> {
        let mut bits = Vec::new();
        for v in [id, 0, 2, 1] {
            ue(&mut bits, v);
        }
        bits.push(false);
        ue(&mut bits, width_mbs - 1);
        ue(&mut bits, height_mbs - 1);
        bits
    }

    /// The size cap holds for EVERY parameter set the decoder sees, not just the record's: a
    /// keyframe may repeat its SPS in band, and a later SPS with the same id replaces the
    /// record's. This one is coded 16400 wide (past `MAX_DIM`) but cropped to 64, so neither
    /// a record-only check nor one on the cropped size would stop it reaching the decoder.
    #[test]
    fn an_oversized_parameter_set_inside_the_keyframe_is_refused_before_decoding() {
        let mut bits = sps_head(0, 1025, 3);
        bits.extend([true, true, true]); // frame_mbs_only, direct_8x8, frame_cropping
        for v in [0, 8168, 0, 0] {
            ue(&mut bits, v); // right crop 8168 x 2: 16400 coded, 64 shown
        }
        bits.extend([false, true]); // no VUI, RBSP stop bit
        let err = frame_png(&high10_input(Some(&baseline_sps(&bits))))
            .expect_err("an over-cap SPS must not decode");
        assert!(
            err.starts_with("refusing"),
            "refused only after decoding: {err}"
        );
    }

    /// The colour rules come from the SPS the picture is decoded WITH. A second SPS under
    /// another id, one nothing in the stream refers to, says full range; the slices still name
    /// id 0 (studio swing), so the colour must not move.
    #[test]
    fn colour_comes_from_the_parameter_set_the_slices_name() {
        let mut bits = sps_head(1, 4, 3);
        bits.extend([true, true, false]); // frame_mbs_only, direct_8x8, no cropping
        bits.push(true); // VUI present
        bits.extend([false, false]); // no aspect ratio, no overscan
        bits.extend([true, true, false, true, true, false]); // signal type: format 5, FULL range
        bits.extend([false; 6]); // chroma loc, timing, NAL/VCL HRD, pic_struct, restriction
        bits.push(true); // RBSP stop bit
        let png = frame_png(&high10_input(Some(&baseline_sps(&bits))))
            .expect("an unused SPS does not stop the decode");
        assert_is_3366cc_at_64x48(&png);
    }

    /// A 4:4:4 stream (the High 4:4:4 Predictive profile issue #35 was about) is refused by
    /// name, never decoded as 4:2:0.
    #[test]
    fn a_444_stream_is_refused() {
        let mkv = include_bytes!("../../../tests/fixtures/h264/high444-3366cc.mkv");
        let input = st2k_codecs::h264::child_input(&mut Cursor::new(&mkv[..]), 0.30)
            .expect("the reader still finds the record and keyframe");
        assert!(frame_png(&input).is_err());
    }
}
