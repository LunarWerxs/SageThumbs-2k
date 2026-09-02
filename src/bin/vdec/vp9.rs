//! The `st2k vp9-frame` decode core: one raw VP9 keyframe → PNG. The CHILD side of
//! `sagethumbs2k_core::vp9::vp9_frame` (see [`super`] for the shared containment story).
//!
//! The parent already did the container work — `mkv::vp9_keyframe` walked the WebM/MKV
//! Cues for the representative keyframe and stripped the Matroska block framing — so what
//! arrives on stdin is exactly what an encoder wrote for one frame (possibly a VP9
//! superframe, which `vp9dec` unpacks itself). This module:
//!
//! 1. pre-parses the VP9 uncompressed keyframe header WITH ITS OWN ~60-line bit reader —
//!    dimensions are refused against `decode::limits::MAX_DIM` BEFORE the decoder
//!    allocates anything, and the header's own `color_space`/`color_range` drive the
//!    YUV→RGB conversion (the `vp9dec::Frame` struct doesn't carry them);
//! 2. decodes via `vp9dec` (pure Rust, all four profiles, verified upstream against the
//!    official conformance corpus — and here against FFmpeg FATE vectors + 20k fuzz
//!    mutations before it was adopted);
//! 3. converts to 8-bit RGBA: 10/12-bit planes are normalized from their FULL native
//!    precision (equivalent to, but slightly more precise than, the right-shift by
//!    `bit_depth - 8`), studio-swing (limited-range) levels are expanded per the header's
//!    `color_range` flag, and the matrix follows the header's `color_space` — BT.709,
//!    BT.601/SMPTE-170, SMPTE-240, or BT.2020 coefficients, with "unknown" resolved by
//!    the HD-size heuristic (≥1280×720 → BT.709, else BT.601). All four chroma layouts
//!    (4:2:0, 4:2:2, 4:4:0, 4:4:4) are handled by the generic `x >> ss_x, y >> ss_y`
//!    lookup; the one declined layout is `CS_RGB` (a profile-1/3 sRGB stream — the plane
//!    order convention differs and real-world files are practically nonexistent).

use std::io::Cursor;

use sagethumbs2k_core::flv::Bits;
use sagethumbs2k_core::vp9::MAX_DIM;
use vp9dec::PlaneData;

// VP9 color_space values (spec §7.2.2). Only the ones this module branches on are named.
const CS_BT_601: u8 = 1;
const CS_BT_709: u8 = 2;
const CS_SMPTE_170: u8 = 3;
const CS_SMPTE_240: u8 = 4;
const CS_BT_2020: u8 = 5;
const CS_RGB: u8 = 7;

/// What the uncompressed keyframe header declares, parsed before any decode allocation.
struct KeyHeader {
    width: u32,
    height: u32,
    color_space: u8,
    /// `true` = full range (0..2^bd-1), `false` = studio swing (16..235 luma, scaled).
    color_range: bool,
}

/// The testable core: raw VP9 keyframe bytes → PNG bytes.
pub(super) fn frame_png(chunk: &[u8]) -> Result<Vec<u8>, String> {
    let hdr = parse_keyframe_header(chunk)?;
    // Refuse absurd geometry BEFORE vp9dec allocates a frame buffer for it. (vp9dec has
    // its own 2^27-luma-sample refusal; this is tighter and matches the shell-wide cap.)
    if hdr.width > MAX_DIM || hdr.height > MAX_DIM {
        return Err(format!(
            "refusing {}x{} frame (cap {MAX_DIM})",
            hdr.width, hdr.height
        ));
    }
    if hdr.color_space == CS_RGB {
        return Err("VP9 sRGB (CS_RGB) streams are not supported".into());
    }
    let mut decoder = vp9dec::Decoder::new();
    let frames = decoder
        .decode_frame(chunk)
        .map_err(|e| format!("VP9 decode: {e:?}"))?;
    // A container chunk may be a superframe (hidden altref + shown frame); take the first
    // frame that is actually displayable.
    let frame = frames
        .into_iter()
        .filter_map(|f| f.frame)
        .next()
        .ok_or("VP9 chunk held no displayable frame")?;
    let (width, height, rgba) = to_rgba(&frame, &hdr)?;
    let img = image::RgbaImage::from_raw(width, height, rgba)
        .ok_or("decoded plane sizes do not match the frame dimensions")?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| format!("PNG encode: {e}"))?;
    Ok(png)
}

/// Convert a decoded frame to 8-bit RGBA (see the module docs for the exact rules).
fn to_rgba(frame: &vp9dec::Frame, hdr: &KeyHeader) -> Result<(u32, u32, Vec<u8>), String> {
    // The decoder is trusted less than its input: re-check the geometry it reports.
    if frame.width == 0 || frame.height == 0 || frame.width > MAX_DIM || frame.height > MAX_DIM {
        return Err(format!(
            "decoder returned a {}x{} frame (cap {MAX_DIM})",
            frame.width, frame.height
        ));
    }
    let (w, h) = (frame.width as usize, frame.height as usize);
    let (ss_x, ss_y) = (frame.subsampling_x as usize, frame.subsampling_y as usize);
    if ss_x > 1 || ss_y > 1 || frame.bit_depth < 8 || frame.bit_depth > 12 {
        return Err("implausible subsampling / bit depth from the decoder".into());
    }
    let (cw, ch) = ((w + ss_x) >> ss_x, (h + ss_y) >> ss_y);
    // Verify the plane geometry instead of trusting it — an inconsistency would otherwise
    // panic on an index below (and this whole process is panic=abort).
    if frame.y.len() != w * h || frame.u.len() != cw * ch || frame.v.len() != cw * ch {
        return Err("decoded plane sizes are inconsistent".into());
    }

    // Matrix coefficients (Kr, Kb) from the header's color_space; VP9 files habitually
    // say "unknown", which resolves by frame size like every player does.
    let (kr, kb) = match hdr.color_space {
        CS_BT_709 => (0.2126f32, 0.0722f32),
        CS_BT_601 | CS_SMPTE_170 => (0.299, 0.114),
        CS_SMPTE_240 => (0.212, 0.087),
        CS_BT_2020 => (0.2627, 0.0593),
        _ => {
            if w >= 1280 || h >= 720 {
                (0.2126, 0.0722) // BT.709 for HD-sized frames
            } else {
                (0.299, 0.114) // BT.601 for SD
            }
        }
    };
    let kg = 1.0 - kr - kb;

    // Level normalization, generic over bit depth: limited (studio) range spans
    // 16..235 for luma and 16..240 for chroma, scaled by 2^(bd-8); full range spans
    // 0..2^bd-1. Chroma is centered at 2^(bd-1) in both. Normalizing from the native
    // depth keeps all 10/12 bits of precision in the float math.
    let bd = u32::from(frame.bit_depth);
    let scale = (1u32 << (bd - 8)) as f32;
    let (y_lo, y_range, c_range) = if hdr.color_range {
        let full = ((1u64 << bd) - 1) as f32;
        (0.0f32, full, full)
    } else {
        (16.0 * scale, 219.0 * scale, 224.0 * scale)
    };
    let c_mid = (1u64 << (bd - 1)) as f32;

    let sample = |p: &PlaneData, i: usize| -> f32 {
        match p {
            PlaneData::U8(v) => f32::from(v[i]),
            PlaneData::U16(v) => f32::from(v[i]),
        }
    };

    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        let crow = (row >> ss_y) * cw;
        for col in 0..w {
            let yy = ((sample(&frame.y, row * w + col) - y_lo) / y_range).clamp(0.0, 1.0);
            let ci = crow + (col >> ss_x);
            let cb = (sample(&frame.u, ci) - c_mid) / c_range;
            let cr = (sample(&frame.v, ci) - c_mid) / c_range;
            let r = yy + 2.0 * (1.0 - kr) * cr;
            let b = yy + 2.0 * (1.0 - kb) * cb;
            let g = (yy - kr * r - kb * b) / kg;
            rgba.push((r.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
            rgba.push((g.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
            rgba.push((b.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
            rgba.push(255);
        }
    }
    Ok((frame.width, frame.height, rgba))
}

/// The frame marker + profile prologue (spec §6.2): the two fields common to every VP9
/// frame, keyframe or not.
///
/// Bit reads go through `sagethumbs2k_core::flv::Bits` — the same MSB-first, bounds-checked
/// reader `flv.rs`'s SPS parser uses — rather than a private duplicate that could drift from
/// it independently.
fn parse_frame_marker_and_profile(b: &mut Bits) -> Result<u8, String> {
    let mut take = |n: u32| b.bits(n).ok_or("truncated VP9 header");
    if take(2)? != 2 {
        return Err("not a VP9 frame (bad frame marker)".into());
    }
    let low = take(1)?;
    let high = take(1)?;
    let profile = (high << 1 | low) as u8;
    if profile == 3 {
        take(1)?; // reserved_zero
    }
    Ok(profile)
}

/// The keyframe-only flags + sync code (spec §6.2). Anything that is not a plain,
/// decodable keyframe (`show_existing_frame`, an inter frame, a bad sync code) is an
/// error: the parent only ever sends keyframes, so those are corruption, not cases to handle.
fn verify_keyframe_flags(b: &mut Bits) -> Result<(), String> {
    let mut take = |n: u32| b.bits(n).ok_or("truncated VP9 header");
    if take(1)? == 1 {
        return Err("show_existing_frame — not a decodable keyframe".into());
    }
    if take(1)? != 0 {
        return Err("not a keyframe".into());
    }
    take(1)?; // show_frame
    take(1)?; // error_resilient_mode
    if take(24)? != 0x49_83_42 {
        return Err("bad VP9 frame sync code".into());
    }
    Ok(())
}

/// The `color_config` field (spec §7.2.2): color space and range, plus the profile-1/3-only
/// subsampling/reserved bits this module does not otherwise use.
fn parse_color_config(b: &mut Bits, profile: u8) -> Result<(u8, bool), String> {
    let mut take = |n: u32| b.bits(n).ok_or("truncated VP9 header");
    if profile >= 2 {
        take(1)?; // ten_or_twelve_bit (the decoder re-derives bit depth itself)
    }
    let color_space = take(3)? as u8;
    let color_range = if color_space != CS_RGB {
        let range = take(1)? == 1;
        if profile == 1 || profile == 3 {
            take(3)?; // subsampling_x, subsampling_y, reserved_zero
        }
        range
    } else {
        if profile == 1 || profile == 3 {
            take(1)?; // reserved_zero
        }
        true // CS_RGB is always full range
    };
    Ok((color_space, color_range))
}

/// `frame_width_minus_1` / `frame_height_minus_1` (spec §6.2).
fn parse_frame_size(b: &mut Bits) -> Result<(u32, u32), String> {
    let mut take = |n: u32| b.bits(n).ok_or("truncated VP9 header");
    let width = take(16)? + 1;
    let height = take(16)? + 1;
    Ok((width, height))
}

/// Parse the fixed-layout head of a VP9 KEYFRAME's uncompressed header (spec §6.2):
/// frame marker, profile, sync code, color config, frame size.
fn parse_keyframe_header(d: &[u8]) -> Result<KeyHeader, String> {
    let mut b = Bits::new(d);
    let profile = parse_frame_marker_and_profile(&mut b)?;
    verify_keyframe_flags(&mut b)?;
    let (color_space, color_range) = parse_color_config(&mut b, profile)?;
    let (width, height) = parse_frame_size(&mut b)?;
    Ok(KeyHeader {
        width,
        height,
        color_space,
        color_range,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bit-pack a VP9 keyframe header prefix: profile-2, 10-bit, BT.709, limited range,
    /// with the given dimensions. Bit-exact to `parse_keyframe_header`'s layout, so these
    /// tests exercise the same walk the hostile path takes.
    fn p2_header(width: u32, height: u32) -> Vec<u8> {
        let mut bits: Vec<u8> = Vec::new();
        let mut put = |v: u32, n: u32| {
            for i in (0..n).rev() {
                bits.push(((v >> i) & 1) as u8);
            }
        };
        put(2, 2); // frame_marker
        put(0, 1); // profile_low_bit  (profile 2 = 0b10 → low 0, high 1)
        put(1, 1); // profile_high_bit
        put(0, 1); // show_existing_frame
        put(0, 1); // frame_type: KEY
        put(1, 1); // show_frame
        put(1, 1); // error_resilient_mode
        put(0x49, 8); // sync code
        put(0x83, 8);
        put(0x42, 8);
        put(0, 1); // ten_or_twelve_bit: 10
        put(CS_BT_709 as u32, 3);
        put(0, 1); // color_range: limited
        put(width - 1, 16);
        put(height - 1, 16);
        while !bits.len().is_multiple_of(8) {
            bits.push(0);
        }
        bits.chunks(8)
            .map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b))
            .collect()
    }

    #[test]
    fn keyframe_header_parses() {
        let hdr = parse_keyframe_header(&p2_header(1920, 1080)).expect("valid header");
        assert_eq!((hdr.width, hdr.height), (1920, 1080));
        assert_eq!(hdr.color_space, CS_BT_709);
        assert!(!hdr.color_range);
    }

    /// GIANT DECLARED DIMENSIONS ARE REFUSED BEFORE ANY DECODE ALLOCATION: the header
    /// parse alone rejects them, so a crafted 65535×65535 header never even reaches
    /// vp9dec (whose own 2^27-sample cap, and the job-object memory cap above that, are
    /// the second and third fences).
    #[test]
    fn giant_dimensions_are_refused_pre_decode() {
        let err = frame_png(&p2_header(65535, 65535)).unwrap_err();
        assert!(err.contains("refusing"), "wrong refusal: {err}");
        assert!(u64::from(MAX_DIM) * u64::from(MAX_DIM) < 65535 * 65535);
    }

    /// Garbage, truncations, and a valid-header-garbage-body frame: every one must come
    /// back `Err`, never panic (this test aborting IS the failure under panic=abort) and
    /// never a giant allocation.
    #[test]
    fn garbage_and_truncations_fail_cleanly() {
        assert!(frame_png(&[]).is_err());
        assert!(frame_png(b"definitely not a vp9 frame").is_err());
        assert!(frame_png(&[0xFF; 64]).is_err());
        // A structurally valid keyframe header with a garbage compressed body.
        let mut fake = p2_header(64, 64);
        fake.extend_from_slice(&[0xA5; 128]);
        assert!(frame_png(&fake).is_err());
        for n in 0..fake.len() {
            let _ = frame_png(&fake[..n]);
        }
    }

    /// The conversion refuses implausible decoder output BEFORE allocating RGBA for it —
    /// [`to_rgba`]'s own geometry check, independent of the header pre-check.
    #[test]
    fn conversion_refuses_out_of_cap_frames() {
        let frame = vp9dec::Frame {
            width: MAX_DIM + 1,
            height: 8,
            bit_depth: 8,
            subsampling_x: 1,
            subsampling_y: 1,
            y: PlaneData::U8(vec![0; 16]),
            u: PlaneData::U8(vec![0; 4]),
            v: PlaneData::U8(vec![0; 4]),
        };
        let hdr = KeyHeader {
            width: MAX_DIM + 1,
            height: 8,
            color_space: CS_BT_709,
            color_range: false,
        };
        assert!(to_rgba(&frame, &hdr).is_err());
    }

    /// Known-value conversion: a flat studio-swing gray in 10-bit must come out mid-gray
    /// through the BT.709 path, and full-range black/white must hit 0/255 exactly.
    #[test]
    fn conversion_levels_are_right() {
        // 2x2, 4:2:0, 10-bit limited: Y = 16<<2 + 219<<2 / 2 ≈ mid (502), chroma neutral 512.
        let mid = |hdr: &KeyHeader, y: u16| {
            let frame = vp9dec::Frame {
                width: 2,
                height: 2,
                bit_depth: 10,
                subsampling_x: 1,
                subsampling_y: 1,
                y: PlaneData::U16(vec![y; 4]),
                u: PlaneData::U16(vec![512; 1]),
                v: PlaneData::U16(vec![512; 1]),
            };
            let (_, _, rgba) = to_rgba(&frame, hdr).expect("convert");
            (rgba[0], rgba[1], rgba[2], rgba[3])
        };
        let limited = KeyHeader {
            width: 2,
            height: 2,
            color_space: CS_BT_709,
            color_range: false,
        };
        // Limited range: 16<<2 = 64 is black, (16+219)<<2 = 940 is white.
        assert_eq!(mid(&limited, 64), (0, 0, 0, 255));
        assert_eq!(mid(&limited, 940), (255, 255, 255, 255));
        let (r, g, b, _) = mid(&limited, 502); // ≈ half of 219<<2 above black
        assert!((125..=130).contains(&r) && r == g && g == b, "{r},{g},{b}");
        // Full range: 0 is black, 1023 is white.
        let full = KeyHeader {
            color_range: true,
            ..limited
        };
        assert_eq!(mid(&full, 0), (0, 0, 0, 255));
        assert_eq!(mid(&full, 1023), (255, 255, 255, 255));
    }

    /// END-TO-END on the real FATE conformance vectors (Profile 2 10-bit 4:2:0 and
    /// Profile 3 12-bit 4:4:4): extract the keyframe with the SAME core walk the parent
    /// uses, decode it here, and get a plausible PNG back. Also covers an 8-bit stream
    /// when the plain corpus sample.webm is VP9. Skips when the corpus is absent (CI).
    #[test]
    fn corpus_vp9_vectors_decode() {
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("test-corpus");
        let mut proved = 0;
        for name in ["sample-vp9p2.webm", "sample-vp9p3.webm", "sample.webm"] {
            let Ok(bytes) = std::fs::read(corpus.join(name)) else {
                eprintln!("corpus_vp9_vectors_decode: no {name} — skipping");
                continue;
            };
            let Some(frame) =
                sagethumbs2k_core::vp9::keyframe_bytes(&mut Cursor::new(&bytes), 0.30)
            else {
                // sample.webm may legitimately be VP8 — the codec gate declining it is
                // correct behaviour, not a failure of this test.
                eprintln!("corpus_vp9_vectors_decode: {name} is not VP9 — skipped");
                continue;
            };
            let png = frame_png(&frame).unwrap_or_else(|e| panic!("{name}: {e}"));
            let img = image::load_from_memory(&png).expect("child output should be a valid PNG");
            assert!(img.width() > 0 && img.height() > 0);
            eprintln!(
                "corpus_vp9_vectors_decode: {name} → {}x{} ({} PNG bytes)",
                img.width(),
                img.height(),
                png.len()
            );
            proved += 1;
        }
        // If the two pinned FATE vectors exist, they must BOTH have decoded.
        if corpus.join("sample-vp9p2.webm").exists() && corpus.join("sample-vp9p3.webm").exists() {
            assert!(
                proved >= 2,
                "the FATE vectors are present but did not decode"
            );
        }
    }
}
