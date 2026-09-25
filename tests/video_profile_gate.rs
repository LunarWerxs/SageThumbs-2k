//! Issue #35, end to end over real files: an H.264 clip in a profile Windows' decoder does
//! not implement (High 4:4:4 Predictive, `-pix_fmt yuv444p`) must be REFUSED before Media
//! Foundation is asked, and its ordinary 8-bit 4:2:0 twin must still decode wherever the
//! decoder is present. Both clips are 3 s of `testsrc` from ffmpeg, 17 KB each. The FLV pair
//! (same encoder settings, muxed into `.flv`) covers the FLV remux branch, where `mf` upstream
//! is trivially true because a real `.flv` yields no mp4/mkv mini-clip to derive it from.
//!
//! Why this matters more than "no thumbnail": on the reporter's Windows 10 22H2 the decoder
//! did not decline the 4:4:4 file, it wedged inside `ReadSample`, and the shell's block-stream
//! tier ran inline on the thumbnail thread, so Explorer's whole thumbnail pipeline hung behind
//! one file until a reboot. Windows 11 declines the same file at once, so this test cannot
//! reproduce the hang; what it pins is that the gate keeps MF out of the loop entirely, which
//! is proven by the grab counter NOT moving rather than by the absence of a picture.
//!
//! One test, four phases, because the grab counter is process-wide and the harness runs test
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

/// One 4:4:4-then-4:2:0 pair. `reason_*` are bytes `mf_undecodable_reason` can read (raw
/// mp4, or an FLV already remuxed to a mini-MP4); `probe_*` are the original files, probed end
/// to end through `core::probe_cover`.
fn assert_444_refused_and_420_decodes(
    reason_444: &[u8],
    probe_444: &[u8],
    reason_420: &[u8],
    probe_420: &[u8],
) {
    // The 4:4:4 clip. No frame, no MF grab, and quickly - the old behaviour on a declining
    // decoder was one 8 s worker timeout per tier.
    let reason = st2k_codecs::vcodec::mf_undecodable_reason(&mut std::io::Cursor::new(reason_444))
        .expect("the fixture must be identified as High 4:4:4 Predictive");
    assert!(reason.contains("244"), "{reason}");

    let grabs_before = st2k_codecs::video::mf_grab_attempts();
    let started = Instant::now();
    let probed = core::probe_cover(probe_444);
    let took = started.elapsed();
    assert_eq!(probed, None, "a 4:4:4 clip must yield no frame");
    assert_eq!(
        st2k_codecs::video::mf_grab_attempts(),
        grabs_before,
        "Media Foundation must not have been asked at all (issue #35)"
    );
    assert!(took < Duration::from_secs(2), "the refusal took {took:?}");

    // The 4:2:0 twin, same encoder, same size, decodable profile. Only where the OS actually
    // has the decoder (a Server image may not), and it must go THROUGH MF.
    assert_eq!(
        st2k_codecs::vcodec::mf_undecodable_reason(&mut std::io::Cursor::new(reason_420)),
        None,
        "High 8-bit 4:2:0 must not be refused"
    );
    let decoder_present = st2k_codecs::video::media_foundation_available()
        && st2k_codecs::vcodec::identify(&mut std::io::Cursor::new(reason_420))
            .and_then(|info| info.subtype)
            .and_then(st2k_codecs::vcodec::decoder_installed)
            == Some(true);
    if !decoder_present {
        eprintln!("no H.264 decoder on this Windows - the decode half is skipped");
        return;
    }
    let grabs_before = st2k_codecs::video::mf_grab_attempts();
    let dims = core::probe_cover(probe_420);
    assert!(
        st2k_codecs::video::mf_grab_attempts() > grabs_before,
        "the 4:2:0 clip must have reached Media Foundation"
    );
    assert_eq!(
        dims,
        Some((320, 240)),
        "the 4:2:0 twin must decode to its frame size"
    );
}

#[test]
fn h264_444_is_refused_before_media_foundation_is_asked_and_420_still_decodes() {
    // Phases 1-2: the MP4 pair. The container's own avcC is readable directly, so the raw
    // file bytes ARE the reason-check bytes.
    let mp4_444 = fixture("h264-high444-320x240.mp4");
    let mp4_420 = fixture("h264-high-320x240.mp4");
    assert_444_refused_and_420_decodes(&mp4_444, &mp4_444, &mp4_420, &mp4_420);

    // Phases 3-4: the FLV pair. `mf_undecodable_reason` cannot read FLV's own tag layout, so
    // the reason-check bytes are the remuxed mini-MP4, while the probe runs on the raw .flv.
    let flv_444 = fixture("h264-high444-320x240.flv");
    let flv_420 = fixture("h264-high-320x240.flv");
    assert_eq!(
        st2k_codecs::vcodec::mf_undecodable_reason(&mut std::io::Cursor::new(&flv_444)),
        None,
        "raw FLV bytes never parse as mp4/mkv, so only the remux can carry the gate"
    );
    let clip_444 = st2k_codecs::flv::keyframe_mini_mp4(&mut std::io::Cursor::new(&flv_444))
        .expect("the 4:4:4 FLV fixture must remux to a mini-MP4");
    let clip_420 = st2k_codecs::flv::keyframe_mini_mp4(&mut std::io::Cursor::new(&flv_420))
        .expect("the 4:2:0 FLV fixture must remux to a mini-MP4");
    assert_444_refused_and_420_decodes(&clip_444, &flv_444, &clip_420, &flv_420);
}
