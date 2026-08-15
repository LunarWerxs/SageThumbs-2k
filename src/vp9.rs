//! VP9 Profile 2/3 (10/12-bit "HDR") thumbnails for WebM/MKV — decoded OUT OF PROCESS.
//!
//! Media Foundation's VP9 support stops at Profile 0/1, measured on this machine with
//! Microsoft's Store VP9 extension 1.2.20.0 installed AND an RTX 4070 Ti present: the GPU
//! vendor decoder is not reachable through `IMFSourceReader`, so a Profile 2 file fails
//! every OS video tier while the file, the container parser, and our registration are all
//! healthy. The pure-Rust `vp9dec` crate decodes all four profiles, but it is a 0.1.x with
//! dozens of unsafe (SIMD) blocks and this crate builds `panic = "abort"` where
//! `catch_unwind` cannot help — the same argument as the FLV Flash codecs — so it is
//! linked ONLY into `st2k.exe` (the EXE-only `vp9-video` feature) and runs in a throwaway
//! child (`st2k vp9-frame`, `src/bin/vdec/vp9.rs`).
//!
//! Division of labour across the process boundary: THIS side (in the shell, pure parsing)
//! finds the keyframe — [`crate::mkv::vp9_keyframe`] reads the container's own Cues index
//! for the frame nearest the user's video offset, falling back to the first cluster — and
//! pipes the raw frame bytes to the child on stdin; the child decodes, converts to RGBA,
//! and answers with a PNG on stdout. No temp file ever holds a frame of the user's video,
//! and a child crash/abort is a clean `None` here (the pre-support behaviour: no tile).
//!
//! ORDERING (the design point): this tier runs LAST, only after every Media Foundation
//! tier came back empty. Profile 0 is the overwhelmingly common case and MF decodes it
//! hardware-accelerated and in-process — routing all VP9 here would make the common case
//! slower and spawn a process per file. The `V_VP9` codec gate inside `vp9_keyframe`
//! means every non-VP9 file skips the tier for the price of a few KB of header reads.

use std::io::{Read, Seek};
use std::time::Duration;

/// Re-exported for the child (`src/bin/vdec/vp9.rs`, a separate bin crate that can't see
/// the `pub(crate)` limits module): the shell-wide dimension cap both ends enforce.
pub use crate::decode::limits::MAX_DIM;

/// Cap on the keyframe bytes handed to the `st2k vp9-frame` child (and on what that child
/// will accept from stdin — the two ends share this constant). A single VP9 intra frame is
/// a few MB even at 4K HDR quality; a "frame" bigger than this is a crafted file.
pub const VP9_INPUT_CAP: usize = 64 * 1024 * 1024;
/// Cap on the PNG read back from the child. The child refuses frames past
/// `decode::limits::MAX_DIM` and PNG shrinks raw RGBA; more means a broken/hostile child.
const VP9_PNG_CAP: usize = 64 * 1024 * 1024;
/// CPU budget for one child decode — one keyframe through a software VP9 decoder is well
/// under a second even at 4K 12-bit, so this is pure headroom — plus the elapsed backstop
/// for a child that hangs without burning CPU on a loaded machine (the same split the
/// ImageMagick and FLV watchdogs use).
const VP9_CPU_BUDGET: Duration = Duration::from_secs(20);
const VP9_WALL_CEILING: Duration = Duration::from_secs(60);

/// The raw VP9 keyframe payload of a Matroska/WebM source, re-exported for the child's
/// corpus tests (the bin crate can't reach the private `crate::mkv`).
pub fn keyframe_bytes<R: Read + Seek>(r: &mut R, fraction: f64) -> Option<Vec<u8>> {
    crate::mkv::vp9_keyframe(r, fraction)
}

/// Decode a representative VP9 keyframe of a Matroska/WebM source to a frame — OUT OF
/// PROCESS via the sibling `st2k.exe` (see the module docs for why). Self-gated on the
/// container's first video track being `V_VP9`; any failure — no sibling exe (DLL-only or
/// feature-less build), hostile input, a child crash/abort, over-budget — is a clean
/// `None`, exactly the pre-support behaviour.
pub(crate) fn vp9_frame<R: Read + Seek>(r: &mut R, fraction: f64) -> Option<image::DynamicImage> {
    let frame = crate::mkv::vp9_keyframe(r, fraction)?;
    if frame.is_empty() || frame.len() > VP9_INPUT_CAP {
        return None;
    }
    let png = crate::flv::child_frame_png(
        "vp9-frame",
        &frame,
        VP9_CPU_BUDGET,
        VP9_WALL_CEILING,
        VP9_PNG_CAP,
    )?;
    // Bounded parse of OUR OWN child's output: the PNG is size-capped above and the child
    // caps its frame at MAX_DIM², so this in-process decode is small by construction; the
    // dimension re-check makes that a verified property, not an assumption.
    let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png).ok()?;
    let max = crate::decode::limits::MAX_DIM;
    if img.width() == 0 || img.height() == 0 || img.width() > max || img.height() > max {
        crate::safety::log_debug("vp9 decode: child returned out-of-bounds dimensions");
        return None;
    }
    Some(img)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::Path;

    /// Junk in, `None` out, without ever reaching a spawn: the track gate must reject
    /// non-VP9 input before the keyframe walk touches a cluster.
    #[test]
    fn vp9_frame_declines_junk_without_spawning() {
        assert!(vp9_frame(&mut Cursor::new(&b"junk"[..]), 0.30).is_none());
        assert!(vp9_frame(&mut Cursor::new(&[0u8; 256][..]), 0.30).is_none());
    }

    /// A real Profile 2 container decodes WHEN the helper is there, and declines cleanly when
    /// it is not. Both are correct, so the test asserts whichever applies rather than a fixed
    /// answer.
    ///
    /// This previously asserted `is_none()` unconditionally, on the reasoning that no
    /// `st2k.exe` sits next to a test binary. That is false in any normal tree: the sibling
    /// lookup finds the one cargo has just built, the spawn succeeds, and the assertion fires
    /// on a decode that WORKED. A test that only passes when the feature is unavailable proves
    /// nothing about the feature, so it is inverted here into the stronger claim.
    #[test]
    fn a_real_profile2_container_decodes_when_the_helper_exists() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("test-corpus")
            .join("sample-vp9p2.webm");
        let Ok(bytes) = std::fs::read(&path) else {
            return; // corpus-gated, like the other sample-backed tests
        };
        let got = vp9_frame(&mut Cursor::new(&bytes), 0.30);
        match crate::sibling_of_dll(crate::CLI_EXE) {
            Some(exe) if exe.exists() => {
                let img = got.expect("the helper is present, so Profile 2 must decode");
                assert!(
                    img.width() > 0 && img.height() > 0,
                    "decoded frame must have real dimensions, got {}x{}",
                    img.width(),
                    img.height(),
                );
            }
            // No helper (a DLL-only or feature-less build): declining is the correct,
            // pre-support behaviour, never a panic or a partial image.
            _ => assert!(
                got.is_none(),
                "without the helper this must decline cleanly"
            ),
        }
    }
}
