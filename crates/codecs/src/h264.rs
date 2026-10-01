//! H.264 that Windows' own decoder cannot open — above all **High 10**, the 10-bit encode
//! anime releases standardised on (issue #52: a VCB-Studio `[Hi10p]` MKV got no thumbnail)
//! — decoded OUT OF PROCESS by the vendored pure-Rust `rust_h264` (`crates/vendor/rust_h264`).
//!
//! Windows' H.264 decoder stops at 8-bit 4:2:0, and on Windows 10 it can wedge instead of
//! failing on the profiles it lacks, which is why `vcodec::mf_undecodable_reason` keeps
//! those files away from Media Foundation altogether (issue #35). Until now that meant no
//! thumbnail at all. `rust_h264` decodes Baseline, Main, High and (with our patch, see
//! `crates/vendor/rust_h264-patches/README.md`) High 10 and the other bit depths up to 14,
//! 4:2:0 only; the 4:2:2 and 4:4:4 profiles are still refused by name.
//!
//! The containment is the VP9 and MPEG tiers' (`crate::vp9`, `crate::mpeg12`): the decoder is
//! a third-party bitstream parser, this crate builds `panic = "abort"`, so it is linked only
//! into `st2k.exe` (the EXE-only `h264-video` feature) and runs in a throwaway child,
//! `st2k h264-frame`, under the 512 MiB job cap. A crash or a refusal is a clean `None` here.
//!
//! Division of labour: THIS side finds one keyframe near the user's video offset and the
//! track's decoder configuration record (`AVCDecoderConfigurationRecord`, the SPS and PPS)
//! with our own Matroska and ISO-BMFF readers, and pipes both to the child as
//! [`child_input`]; the child decodes, converts to RGBA and answers with a PNG on stdout.
//!
//! ORDERING: this tier runs after every Media Foundation tier and after VP9 and MPEG, so an
//! ordinary 8-bit file keeps MF's hardware-accelerated in-process decode and only an
//! otherwise blank tile pays for a process spawn.

use std::io::{Read, Seek};
use std::time::Duration;

/// Re-exported for the child (`src/bin/vdec/h264.rs`, a separate bin crate that can't see
/// the `pub(crate)` limits module): the shell-wide dimension cap both ends enforce.
pub use crate::decode::limits::MAX_DIM;

/// Cap on what is handed to the `st2k h264-frame` child (and on what that child will accept
/// from stdin — the two ends share this constant). One intra frame is a few MB even at 4K;
/// more than this is a crafted file.
pub const H264_INPUT_CAP: usize = 64 * 1024 * 1024;
/// Cap on the PNG read back from the child, which refuses frames past `MAX_DIM`.
const H264_PNG_CAP: usize = 64 * 1024 * 1024;
/// CPU budget for one child decode — one software-decoded intra frame is well under a second
/// at 1080p — plus the elapsed backstop for a child that hangs without burning CPU (the split
/// the ImageMagick, FLV and VP9 watchdogs use).
const H264_CPU_BUDGET: Duration = Duration::from_secs(20);
const H264_WALL_CEILING: Duration = Duration::from_secs(60);

/// The child's stdin for the keyframe nearest `fraction` of an H.264 Matroska or MP4/MOV
/// source: the decoder configuration record's length (u32, little-endian), the record, then
/// the keyframe's length-prefixed NAL units. `None` for any other codec or container, and
/// past [`H264_INPUT_CAP`]. Public so the child's corpus tests build exactly what it gets.
pub fn child_input<R: Read + Seek>(r: &mut R, fraction: f64) -> Option<Vec<u8>> {
    let (config, frame) = crate::mkv::h264_keyframe(r, fraction)
        .or_else(|| crate::mp4::h264_keyframe(r, fraction))?;
    let len = u32::try_from(config.len()).ok()?;
    let total = 4usize.checked_add(config.len())?.checked_add(frame.len())?;
    if config.is_empty() || frame.is_empty() || total > H264_INPUT_CAP {
        return None;
    }
    let mut input = Vec::with_capacity(total);
    input.extend_from_slice(&len.to_le_bytes());
    input.extend_from_slice(&config);
    input.extend_from_slice(&frame);
    Some(input)
}

/// Split [`child_input`] back into `(configuration record, keyframe)`: the child's side.
pub fn split_child_input(input: &[u8]) -> Option<(&[u8], &[u8])> {
    let len = u32::from_le_bytes(input.get(..4)?.try_into().ok()?) as usize;
    let end = 4usize.checked_add(len)?;
    let config = input.get(4..end)?;
    let frame = input.get(end..)?;
    (!config.is_empty() && !frame.is_empty()).then_some((config, frame))
}

/// Decode a representative H.264 keyframe of a Matroska or MP4/MOV source to a frame — OUT OF
/// PROCESS via the sibling `st2k.exe` (see the module docs for why). Self-gated on the video
/// track being H.264; any failure — no sibling exe (DLL-only or feature-less build), a 4:2:2
/// or 4:4:4 stream, hostile input, a child crash, over budget — is a clean `None`, exactly
/// the behaviour before this tier existed.
pub(crate) fn h264_frame<R: Read + Seek>(r: &mut R, fraction: f64) -> Option<image::DynamicImage> {
    let input = child_input(r, fraction)?;
    let png = crate::flv::child_frame_png(
        "h264-frame",
        &input,
        &[],
        H264_CPU_BUDGET,
        H264_WALL_CEILING,
        H264_PNG_CAP,
    )?;
    // Bounded parse of OUR OWN child's output: the PNG is size-capped above and the child
    // caps its frame at MAX_DIM², so this in-process decode is small by construction; the
    // dimension re-check makes that a verified property, not an assumption.
    let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png).ok()?;
    if img.width() == 0 || img.height() == 0 || img.width() > MAX_DIM || img.height() > MAX_DIM {
        st2k_base::safety::log_debug("h264 decode: child returned out-of-bounds dimensions");
        return None;
    }
    Some(img)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Junk in, `None` out, without ever reaching a spawn: neither container reader finds an
    /// H.264 track, so the tier must decline before building any input.
    #[test]
    fn h264_frame_declines_junk_without_spawning() {
        assert!(child_input(&mut Cursor::new(&b"junk"[..]), 0.30).is_none());
        assert!(h264_frame(&mut Cursor::new(&[0u8; 256][..]), 0.30).is_none());
    }

    /// The framing round-trips, and a truncated or empty half is refused rather than handed
    /// to the decoder as a different record.
    #[test]
    fn the_child_input_framing_round_trips_and_refuses_truncation() {
        let mut input = 3u32.to_le_bytes().to_vec();
        input.extend_from_slice(b"cfgFRAME");
        assert_eq!(
            split_child_input(&input),
            Some((&b"cfg"[..], &b"FRAME"[..]))
        );
        assert_eq!(split_child_input(&input[..6]), None, "record cut short");
        assert_eq!(
            split_child_input(&input[..7]),
            None,
            "no frame after the record"
        );
        assert_eq!(
            split_child_input(&[9, 0, 0, 0, 1]),
            None,
            "length past the end"
        );
    }
}
