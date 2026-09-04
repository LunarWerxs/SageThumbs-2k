//! Issue #35, end to end over real files: an H.264 clip in a profile Windows' decoder does
//! not implement (High 4:4:4 Predictive, `-pix_fmt yuv444p`) must be REFUSED before Media
//! Foundation is asked, and its ordinary 8-bit 4:2:0 twin must still decode wherever the
//! decoder is present. Both clips are 3 s of `testsrc` from ffmpeg, 17 KB each.
//!
//! Why this matters more than "no thumbnail": on the reporter's Windows 10 22H2 the decoder
//! did not decline the 4:4:4 file, it wedged inside `ReadSample`, and the shell's block-stream
//! tier ran inline on the thumbnail thread, so Explorer's whole thumbnail pipeline hung behind
//! one file until a reboot. Windows 11 declines the same file at once, so this test cannot
//! reproduce the hang; what it pins is that the gate keeps MF out of the loop entirely, which
//! is proven by the grab counter NOT moving rather than by the absence of a picture.
//!
//! One test, two phases, because the grab counter is process-wide and the harness runs test
//! functions in parallel.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use sagethumbs2k_core as core;

fn fixture(name: &str) -> Vec<u8> {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "tests",
        "fixtures",
        "video",
        name,
    ]
    .iter()
    .collect();
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn h264_444_is_refused_before_media_foundation_is_asked_and_420_still_decodes() {
    // Phase 1: the 4:4:4 clip. No frame, no MF grab, and quickly - the old behaviour on a
    // declining decoder was one 8 s worker timeout per tier.
    let bytes = fixture("h264-high444-320x240.mp4");
    let reason = core::vcodec::mf_undecodable_reason(&mut std::io::Cursor::new(&bytes))
        .expect("the fixture must be identified as High 4:4:4 Predictive");
    assert!(reason.contains("244"), "{reason}");

    let grabs_before = core::video::mf_grab_attempts();
    let started = Instant::now();
    let probed = core::probe_cover(&bytes);
    let took = started.elapsed();
    assert_eq!(probed, None, "a 4:4:4 clip must yield no frame");
    assert_eq!(
        core::video::mf_grab_attempts(),
        grabs_before,
        "Media Foundation must not have been asked at all (issue #35)"
    );
    assert!(took < Duration::from_secs(2), "the refusal took {took:?}");

    // Phase 2: the 4:2:0 twin, same encoder, same size, decodable profile. Only where the
    // OS actually has the decoder (a Server image may not), and it must go THROUGH MF.
    let bytes = fixture("h264-high-320x240.mp4");
    assert_eq!(
        core::vcodec::mf_undecodable_reason(&mut std::io::Cursor::new(&bytes)),
        None,
        "High 8-bit 4:2:0 must not be refused"
    );
    let decoder_present = core::video::media_foundation_available()
        && core::vcodec::identify(&mut std::io::Cursor::new(&bytes))
            .and_then(|info| info.subtype)
            .and_then(core::vcodec::decoder_installed)
            == Some(true);
    if !decoder_present {
        eprintln!("no H.264 decoder on this Windows - the decode half is skipped");
        return;
    }
    let grabs_before = core::video::mf_grab_attempts();
    let dims = core::probe_cover(&bytes);
    assert!(
        core::video::mf_grab_attempts() > grabs_before,
        "the 4:2:0 clip must have reached Media Foundation"
    );
    assert_eq!(
        dims,
        Some((320, 240)),
        "the 4:2:0 twin must decode to its frame size"
    );
}
