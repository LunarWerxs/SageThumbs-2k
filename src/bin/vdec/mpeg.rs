//! The `st2k mpeg-frame` decode core: one MPEG-1/2 intra-picture unit → PNG. The CHILD side
//! of `sagethumbs2k_core::mpeg12::mpeg_frame` (see [`super`] for the shared containment
//! story).
//!
//! The parent already did the container work — `mpeg12::intra_slice_bytes` demuxed the
//! program stream and cut ONE unit: the sequence header with its extensions, an optional GOP
//! header, and a single I-picture (or an I field pair) — so what arrives on stdin is a
//! complete, self-contained elementary stream that decodes to exactly one frame. This module:
//!
//! 1. pre-parses the sequence header's `horizontal_size` / `vertical_size` (12 bits each,
//!    plus the MPEG-2 `sequence_extension`'s two high bits) and refuses anything past
//!    `mpeg12::MPEG_MAX_DIM` BEFORE the decoder allocates a frame for it, and reads the
//!    `sequence_display_extension`'s `matrix_coefficients` when the stream carries one;
//! 2. decodes via `oxideav-mpeg12video` (pure Rust, no `unsafe` in the decoder crate;
//!    verified here against the corpus's MPEG-1 and MPEG-2 streams and 20k mutations before
//!    it was adopted, 0 panics, worst case 135 ms). The one syntax 0.0.13 declines is a
//!    slice that starts mid-row (a first `macroblock_address_increment` above 1, legal per
//!    H.262 section 6.3.17.1): conformance streams use it, none of 20 real-world DVD / VCD /
//!    broadcast files measured on 2026-09-17 did, and such a file simply yields no frame;
//! 3. converts the 8-bit planes to RGBA: studio-swing levels (16..235 / 16..240, the only
//!    range MPEG-1/2 define), the matrix from the display extension when present, else
//!    BT.601 for MPEG-1 (its only defined space) and standard-definition MPEG-2, BT.709 for
//!    HD-sized MPEG-2 frames — the same size heuristic every player uses. 4:2:0, 4:2:2 and
//!    4:4:4 are handled by the generic `x >> ss_x, y >> ss_y` lookup.

use std::io::Cursor;

use oxideav_mpeg12video::sequence_extension::ChromaFormat;
use oxideav_mpeg12video::{decode_video_sequence, PictureCodingType};
use sagethumbs2k_core::flv::Bits;
use sagethumbs2k_core::mpeg12::MPEG_MAX_DIM;

// `matrix_coefficients` (Table 6-9 / ITU-T H.273) values this module branches on.
const MC_BT709: u8 = 1;
const MC_BT601_FCC: u8 = 4;
const MC_BT470BG: u8 = 5;
const MC_SMPTE170: u8 = 6;
const MC_SMPTE240: u8 = 7;
// `extension_start_code_identifier` values (Table 6-2).
const EXT_SEQUENCE: u8 = 1;
const EXT_SEQUENCE_DISPLAY: u8 = 2;

/// What the sequence layer declares, parsed before any decode allocation.
struct SeqHeader {
    width: u32,
    height: u32,
    /// `matrix_coefficients` from a `sequence_display_extension`, when the stream has one.
    matrix: Option<u8>,
    /// Whether a `sequence_extension` follows the header (MPEG-2) or not (MPEG-1).
    mpeg2: bool,
}

/// The testable core: one elementary-stream unit → PNG bytes.
pub(super) fn frame_png(unit: &[u8]) -> Result<Vec<u8>, String> {
    let hdr = parse_sequence_layer(unit)?;
    // Refuse absurd geometry BEFORE the decoder allocates a frame buffer for it (see
    // `MPEG_MAX_DIM` for why the fence is tighter than the shell-wide cap).
    if hdr.width == 0 || hdr.height == 0 || hdr.width > MPEG_MAX_DIM || hdr.height > MPEG_MAX_DIM {
        return Err(format!(
            "refusing {}x{} frame (cap {MPEG_MAX_DIM})",
            hdr.width, hdr.height
        ));
    }
    let frames = decode_video_sequence(unit).map_err(|e| format!("MPEG decode: {e:?}"))?;
    // The unit holds one intra picture (or a field pair the decoder assembled into one
    // frame). Prefer the intra frame should a decoder ever hand back more than one.
    let decoded = frames
        .iter()
        .find(|f| matches!(f.picture_coding_type, PictureCodingType::Intra))
        .or_else(|| frames.first())
        .ok_or("MPEG unit decoded to no frame")?;
    let (width, height, rgba) = to_rgba(&decoded.frame, &hdr)?;
    let img = image::RgbaImage::from_raw(width, height, rgba)
        .ok_or("decoded plane sizes do not match the frame dimensions")?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| format!("PNG encode: {e}"))?;
    Ok(png)
}

/// Convert a decoded frame to 8-bit RGBA (see the module docs for the exact rules).
fn to_rgba(
    frame: &oxideav_mpeg12video::frame_assembly::FrameBuffer,
    hdr: &SeqHeader,
) -> Result<(u32, u32, Vec<u8>), String> {
    // The decoder is trusted less than its input: re-check the geometry it reports.
    let (w, h) = (frame.width, frame.height);
    if w == 0 || h == 0 || w > MPEG_MAX_DIM as usize || h > MPEG_MAX_DIM as usize {
        return Err(format!(
            "decoder returned a {w}x{h} frame (cap {MPEG_MAX_DIM})"
        ));
    }
    let (ss_x, ss_y) = match frame.chroma_format {
        ChromaFormat::Yuv420 => (1usize, 1usize),
        ChromaFormat::Yuv422 => (1, 0),
        ChromaFormat::Yuv444 => (0, 0),
    };
    let (cw, ch) = frame.visible_chroma_dims();
    if cw == 0 || ch == 0 || cw < (w + ss_x) >> ss_x || ch < (h + ss_y) >> ss_y {
        return Err("implausible chroma geometry from the decoder".into());
    }
    // `packed_rect` clips to the plane, so verify the copies are the size the geometry says
    // before indexing them — an inconsistency would otherwise panic below (panic=abort).
    let y = frame.y.packed_rect(w, h);
    let cb = frame.cb.packed_rect(cw, ch);
    let cr = frame.cr.packed_rect(cw, ch);
    if y.len() != w * h || cb.len() != cw * ch || cr.len() != cw * ch {
        return Err("decoded plane sizes are inconsistent".into());
    }

    // Matrix coefficients (Kr, Kb): the display extension when the stream says, else
    // MPEG-1 is BT.601 by definition and MPEG-2 resolves by frame size like every player.
    let (kr, kb) = match hdr.matrix {
        Some(MC_BT709) => (0.2126f32, 0.0722f32),
        Some(MC_BT601_FCC | MC_BT470BG | MC_SMPTE170) => (0.299, 0.114),
        Some(MC_SMPTE240) => (0.212, 0.087),
        _ if hdr.mpeg2 && (w >= 1280 || h >= 720) => (0.2126, 0.0722),
        _ => (0.299, 0.114),
    };
    let kg = 1.0 - kr - kb;

    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        let crow = (row >> ss_y) * cw;
        for col in 0..w {
            // Studio swing: 16..235 luma, 16..240 chroma, the only levels MPEG-1/2 define.
            let yy = ((f32::from(y[row * w + col]) - 16.0) / 219.0).clamp(0.0, 1.0);
            let ci = crow + (col >> ss_x);
            let pb = (f32::from(cb[ci]) - 128.0) / 224.0;
            let pr = (f32::from(cr[ci]) - 128.0) / 224.0;
            let r = yy + 2.0 * (1.0 - kr) * pr;
            let b = yy + 2.0 * (1.0 - kb) * pb;
            let g = (yy - kr * r - kb * b) / kg;
            rgba.push((r.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
            rgba.push((g.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
            rgba.push((b.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
            rgba.push(255);
        }
    }
    Ok((w as u32, h as u32, rgba))
}

/// Position of the first `00 00 01 xx` at or after `from` whose `xx` satisfies `want`.
fn find_start_code(es: &[u8], from: usize, want: impl Fn(u8) -> bool) -> Option<usize> {
    let mut i = from;
    while i + 4 <= es.len() {
        if es[i] == 0 && es[i + 1] == 0 && es[i + 2] == 1 && want(es[i + 3]) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Parse the sequence layer: the header's 12-bit sizes (§6.2.2.1), extended by the MPEG-2
/// `sequence_extension`'s two high bits each (§6.2.2.3) when one follows, and the optional
/// `sequence_display_extension`'s colour description (§6.2.2.4).
///
/// Bit reads go through `sagethumbs2k_core::flv::Bits` — the same MSB-first, bounds-checked
/// reader the SPS and VP9 header parsers use — rather than a private duplicate.
fn parse_sequence_layer(unit: &[u8]) -> Result<SeqHeader, String> {
    let seq = find_start_code(unit, 0, |c| c == 0xB3).ok_or("no sequence header")?;
    let body = unit.get(seq + 4..).ok_or("truncated sequence header")?;
    let mut b = Bits::new(body);
    let mut take = |n: u32| b.bits(n).ok_or("truncated sequence header");
    let mut hdr = SeqHeader {
        width: take(12)?,
        height: take(12)?,
        matrix: None,
        mpeg2: false,
    };
    // Walk the extension blocks that follow the header, up to the first GOP / picture.
    let mut pos = seq + 4;
    while let Some(ext) = find_start_code(unit, pos, |_| true) {
        if unit[ext + 3] != 0xB5 {
            break;
        }
        apply_extension(&mut hdr, &unit[ext + 4..])?;
        pos = ext + 4;
    }
    Ok(hdr)
}

/// Fold one `extension_start_code` block into the header: the `sequence_extension` (MPEG-2;
/// the two high bits of each size) or the `sequence_display_extension` (colour description).
/// Every other extension id is skipped.
fn apply_extension(hdr: &mut SeqHeader, body: &[u8]) -> Result<(), String> {
    let mut eb = Bits::new(body);
    match eb.bits(4).ok_or("truncated extension")? as u8 {
        EXT_SEQUENCE => sequence_extension(hdr, &mut eb),
        EXT_SEQUENCE_DISPLAY => display_extension(hdr, &mut eb),
        _ => Ok(()),
    }
}

/// `sequence_extension()` (§6.2.2.3), after its 4-bit id: the fields up to and including
/// the two size-extension pairs.
fn sequence_extension(hdr: &mut SeqHeader, eb: &mut Bits<'_>) -> Result<(), String> {
    let mut e = |n: u32| eb.bits(n).ok_or("truncated sequence_extension");
    hdr.mpeg2 = true;
    e(8)?; // profile_and_level_indication
    e(1)?; // progressive_sequence
    e(2)?; // chroma_format
    hdr.width |= e(2)? << 12;
    hdr.height |= e(2)? << 12;
    Ok(())
}

/// `sequence_display_extension()` (§6.2.2.4), after its 4-bit id: the colour description,
/// when the stream carries one.
fn display_extension(hdr: &mut SeqHeader, eb: &mut Bits<'_>) -> Result<(), String> {
    let mut e = |n: u32| eb.bits(n).ok_or("truncated sequence_display_extension");
    e(3)?; // video_format
    if e(1)? == 1 {
        e(8)?; // colour_primaries
        e(8)?; // transfer_characteristics
        hdr.matrix = Some(e(8)? as u8);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An MPEG-2 sequence header + extension pair declaring the given size (12 + 2 bits
    /// each), plus a display extension declaring BT.709 — bit-exact to the layout
    /// `parse_sequence_layer` walks.
    fn mpeg2_prelude(width: u32, height: u32) -> Vec<u8> {
        let mut out = vec![0x00, 0x00, 0x01, 0xB3];
        out.push((width >> 4) as u8);
        out.push((((width & 0xF) << 4) | ((height >> 8) & 0xF)) as u8);
        out.push((height & 0xFF) as u8);
        out.extend_from_slice(&[0x13, 0xFF, 0xFF, 0xE0, 0x18]);
        // sequence_extension: id 1 | profile_and_level 0x48 (Main@Main) | progressive 0,
        // chroma 01, horizontal_size_extension (2), vertical_size_extension (2), then the
        // bit-rate extension / marker / vbv / low_delay / frame-rate extension fields.
        let hx = ((width >> 12) & 3) as u8;
        let vx = ((height >> 12) & 3) as u8;
        out.extend_from_slice(&[
            0x00,
            0x00,
            0x01,
            0xB5,
            0x14,
            0x82 | (hx >> 1),
            ((hx & 1) << 7) | (vx << 5),
            0x01,
            0x00,
            0x00,
        ]);
        // sequence_display_extension: id 2 | video_format 0 | colour_description 1, then
        // colour_primaries 1, transfer_characteristics 1, matrix_coefficients 1 (BT.709),
        // display sizes.
        out.extend_from_slice(&[
            0x00, 0x00, 0x01, 0xB5, 0x21, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00,
        ]);
        out
    }

    #[test]
    fn sequence_layer_parses_sizes_extensions_and_matrix() {
        let hdr = parse_sequence_layer(&mpeg2_prelude(1920, 1080)).expect("valid");
        assert_eq!((hdr.width, hdr.height), (1920, 1080));
        assert!(hdr.mpeg2);
        assert_eq!(hdr.matrix, Some(MC_BT709));
        // MPEG-1: header only, no extension → no matrix, not mpeg2, 12-bit sizes.
        let m1 = &mpeg2_prelude(352, 288)[..12];
        let hdr = parse_sequence_layer(m1).expect("valid");
        assert_eq!((hdr.width, hdr.height), (352, 288));
        assert!(!hdr.mpeg2 && hdr.matrix.is_none());
        // The size extension bits land above bit 12.
        let hdr = parse_sequence_layer(&mpeg2_prelude(4096 + 64, 8192 + 32)).expect("valid");
        assert_eq!((hdr.width, hdr.height), (4160, 8224));
    }

    /// GIANT DECLARED DIMENSIONS ARE REFUSED BEFORE ANY DECODE ALLOCATION: the sequence
    /// layer parse alone rejects them, so a crafted 16383x16383 header (the format's
    /// ceiling, 400 MB of planes) never reaches the decoder, and neither does one a single
    /// sample over the MPEG cap (the job-object memory cap above that is the second fence).
    #[test]
    fn giant_dimensions_are_refused_pre_decode() {
        for (w, h) in [
            (16383, 16383),
            (MPEG_MAX_DIM + 1, 16),
            (16, MPEG_MAX_DIM + 1),
            (0, 480),
        ] {
            let err = frame_png(&mpeg2_prelude(w, h)).unwrap_err();
            assert!(err.contains("refusing"), "{w}x{h}: wrong refusal: {err}");
        }
        assert!(u64::from(MPEG_MAX_DIM) * u64::from(MPEG_MAX_DIM) < 16383 * 16383);
    }

    /// Garbage, truncations, and a valid-header-garbage-body unit: every one must come back
    /// `Err`, never panic (this test aborting IS the failure under panic=abort) and never a
    /// giant allocation.
    #[test]
    fn garbage_and_truncations_fail_cleanly() {
        assert!(frame_png(&[]).is_err());
        assert!(frame_png(b"definitely not an mpeg stream").is_err());
        assert!(frame_png(&[0xFF; 64]).is_err());
        let mut fake = mpeg2_prelude(64, 48);
        fake.extend_from_slice(&[0x00, 0x00, 0x01, 0xB8, 0x00, 0x08, 0x00, 0x40]);
        fake.extend_from_slice(&[0x00, 0x00, 0x01, 0x00, 0x00, 0x0F, 0xFF, 0xF8]);
        fake.extend_from_slice(&[0x00, 0x00, 0x01, 0xB5, 0x8F, 0xFF, 0xF3, 0x98, 0x00]);
        fake.extend_from_slice(&[0x00, 0x00, 0x01, 0x01]);
        fake.extend_from_slice(&[0xA5; 128]);
        assert!(frame_png(&fake).is_err());
        for n in 0..fake.len() {
            let _ = frame_png(&fake[..n]);
        }
    }

    /// Known-value conversion: studio-swing black and white hit 0/255 exactly, and a
    /// mid-grey lands in the middle, through both matrices.
    #[test]
    fn conversion_levels_are_right() {
        use oxideav_mpeg12video::frame_assembly::FrameBuffer;
        let flat = |luma: u8, hdr: &SeqHeader| {
            let mut fb = FrameBuffer::new(2, 2, ChromaFormat::Yuv420);
            for yy in 0..2 {
                for xx in 0..2 {
                    fb.y.put_sample(xx, yy, luma);
                }
            }
            fb.cb.put_sample(0, 0, 128);
            fb.cr.put_sample(0, 0, 128);
            let (_, _, rgba) = to_rgba(&fb, hdr).expect("convert");
            (rgba[0], rgba[1], rgba[2], rgba[3])
        };
        let sd = SeqHeader {
            width: 2,
            height: 2,
            matrix: None,
            mpeg2: false,
        };
        assert_eq!(flat(16, &sd), (0, 0, 0, 255));
        assert_eq!(flat(235, &sd), (255, 255, 255, 255));
        let (r, g, b, _) = flat(126, &sd);
        assert!((126..=130).contains(&r) && r == g && g == b, "{r},{g},{b}");
        let hd = SeqHeader {
            matrix: Some(MC_BT709),
            mpeg2: true,
            ..sd
        };
        assert_eq!(flat(16, &hd), (0, 0, 0, 255));
        assert_eq!(flat(235, &hd), (255, 255, 255, 255));
    }

    /// END-TO-END on the corpus: extract the unit with the SAME core walk the parent uses
    /// (`mpeg12::intra_slice_bytes`), decode it here, and get a plausible PNG back. Covers
    /// MPEG-2 ES, MPEG-1 system stream, MPEG-2 program streams and the real MPEG-1 ES.
    /// Skips when the corpus is absent (CI).
    #[test]
    fn corpus_mpeg_streams_decode() {
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("test-corpus");
        let mut proved = 0;
        for (name, w, h) in [
            ("sample.mpg", 640, 360),
            ("sample.m2v", 640, 360),
            ("sample.mpeg", 640, 360),
            ("sample.vob", 640, 360),
            ("real.m2v", 192, 240),
            ("real.vob", 720, 480),
            ("real.m1v", 160, 120),
            ("real-vcd.mpg", 160, 120),
            ("real-es.m2v", 720, 576),
        ] {
            let Ok(bytes) = std::fs::read(corpus.join(name)) else {
                eprintln!("corpus_mpeg_streams_decode: no {name} — skipping");
                continue;
            };
            let unit = sagethumbs2k_core::mpeg12::intra_slice_bytes(&mut Cursor::new(&bytes), 0.30)
                .unwrap_or_else(|| panic!("{name}: no intra unit"));
            let png = frame_png(&unit).unwrap_or_else(|e| panic!("{name}: {e}"));
            let img = image::load_from_memory(&png).expect("child output should be a valid PNG");
            assert_eq!((img.width(), img.height()), (w, h), "{name}");
            // A real picture, not a flat field: the luma must vary.
            let g = img.to_luma8();
            let (min, max) = g
                .pixels()
                .fold((255u8, 0u8), |(lo, hi), p| (lo.min(p[0]), hi.max(p[0])));
            assert!(
                max - min > 32,
                "{name}: decoded frame is flat ({min}..{max})"
            );
            eprintln!(
                "corpus_mpeg_streams_decode: {name} → {}x{} ({} PNG bytes)",
                img.width(),
                img.height(),
                png.len()
            );
            proved += 1;
        }
        if corpus.join("sample.mpeg").exists() && corpus.join("sample.vob").exists() {
            assert!(
                proved >= 2,
                "the corpus streams are present but did not decode"
            );
        }
    }
}
