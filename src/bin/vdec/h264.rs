//! The `st2k h264-frame` decode core: an H.264 decoder configuration record plus one keyframe
//! → PNG. The CHILD side of `st2k_codecs::h264::h264_frame` (see [`super`] for the shared
//! containment story; issue #52 for why it exists).
//!
//! 1. Splits the parent's framing (`h264::split_child_input`) and reads the SPS out of the
//!    configuration record BEFORE decoding: the dimensions are refused against
//!    `decode::limits::MAX_DIM` before the decoder allocates a picture, and the SPS's VUI
//!    (`video_full_range_flag`, `matrix_coefficients`) drives the colour conversion.
//! 2. Decodes with the vendored `rust_h264` (`crates/vendor/rust_h264`, with the high bit depth
//!    patch in `crates/vendor/rust_h264-patches`): the record's SPS and PPS, then the
//!    keyframe's NAL units, then a flush for the picture.
//! 3. Converts to 8-bit RGBA the way the VP9 child does: samples normalised from their native
//!    depth (luma and chroma depths may differ), studio swing expanded unless the stream says
//!    full range, and the matrix the stream names (BT.709, BT.601, SMPTE 240M, BT.2020), with
//!    "unspecified" resolved by frame size like every player (≥1280×720 → BT.709).
//! 4. Stretches the width to the sample aspect ratio the SPS gives, so an anamorphic DVD-sized
//!    encode (720x480 at 8:9) comes out at the 4:3 it is shown at.

use rust_h264::decoder::{Frame, OrderedDecoder};
use rust_h264::nal::{parse_avcc, parse_avcc_config};
use rust_h264::sps::{parse_sps, Sps};
use st2k_codecs::h264::{split_child_input, MAX_DIM};

/// The testable core: the parent's framed input → PNG bytes.
pub(super) fn frame_png(input: &[u8]) -> Result<Vec<u8>, String> {
    let (config, keyframe) = split_child_input(input).ok_or("malformed child input")?;
    let avcc = parse_avcc_config(config).map_err(|e| format!("avcC: {e}"))?;
    let sps_nal = avcc
        .sps_nals
        .first()
        .ok_or("the avcC record carries no SPS")?;
    let sps = parse_sps(&sps_nal.rbsp).map_err(|e| format!("SPS: {e}"))?;
    let (w, h) = (sps.width(), sps.height());
    if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM {
        return Err(format!("refusing {w}x{h} frame (cap {MAX_DIM})"));
    }
    let picture = decode(&avcc.sps_nals, &avcc.pps_nals, keyframe, avcc.length_size)?;
    let rgba = to_rgba(&picture, &sps)?;
    let (width, rgba) =
        to_display_aspect(picture.width, picture.height, rgba, sps.sample_aspect_ratio)?;
    super::encode_png(width, picture.height, rgba)
}

/// Stretch the picture to the shape it is meant to be SHOWN at. A DVD-sized encode stores
/// 720x480 with non-square pixels (8:9 for 4:3, 32:27 for 16:9) and says so in the SPS; the
/// reporter's file (issue #52) is one, and drawn square it is a 3:2 picture squeezed sideways.
/// Only the width moves, so the height stays the stream's own.
fn to_display_aspect(
    w: u32,
    h: u32,
    rgba: Vec<u8>,
    (sar_w, sar_h): (u16, u16),
) -> Result<(u32, Vec<u8>), String> {
    if sar_w == sar_h || sar_w == 0 || sar_h == 0 {
        return Ok((w, rgba));
    }
    let shown = (u64::from(w) * u64::from(sar_w) + u64::from(sar_h) / 2) / u64::from(sar_h);
    let shown = u32::try_from(shown).unwrap_or(MAX_DIM).clamp(1, MAX_DIM);
    let img = image::RgbaImage::from_raw(w, h, rgba).ok_or("RGBA size does not match the frame")?;
    let out = image::imageops::resize(&img, shown, h, image::imageops::FilterType::Triangle);
    Ok((shown, out.into_raw()))
}

/// Feed the parameter sets, then the keyframe's NAL units, and return the first picture.
///
/// An error on one NAL unit does not end the decode: a keyframe access unit can carry SEI,
/// AUD or filler units that matter to nobody here. Only "no picture came out" is a failure,
/// reported with the last error seen, which is the one that explains it.
fn decode(
    sps: &[rust_h264::nal::NalUnit<'_>],
    pps: &[rust_h264::nal::NalUnit<'_>],
    keyframe: &[u8],
    length_size: usize,
) -> Result<Frame, String> {
    let mut decoder = OrderedDecoder::new();
    let mut frames = Vec::new();
    let mut last_error = None;
    let units = parse_avcc(keyframe, length_size);
    for nal in sps.iter().chain(pps).chain(units.iter()) {
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
        let mkv = include_bytes!("../../../tests/fixtures/h264/high10-3366cc.mkv");
        let input = st2k_codecs::h264::child_input(&mut Cursor::new(&mkv[..]), 0.30)
            .expect("the reader finds the H.264 track's record and keyframe");
        let png = frame_png(&input).expect("High 10 decodes");
        let img = image::load_from_memory(&png).expect("PNG").to_rgba8();
        assert_eq!((img.width(), img.height()), (64, 48));
        let px = img.get_pixel(32, 24).0;
        for (got, want) in px.iter().zip([0x33u8, 0x66, 0xcc]) {
            assert!(
                got.abs_diff(want) <= 3,
                "centre pixel {px:?}, want about (51, 102, 204)"
            );
        }
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
