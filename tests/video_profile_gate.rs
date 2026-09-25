//! Issue #35, end to end over real files: an H.264 clip in a profile Windows' decoder does
//! not implement (High 4:4:4 Predictive, `-pix_fmt yuv444p`) must be REFUSED before Media
//! Foundation is asked, and its ordinary 8-bit 4:2:0 twin must still decode wherever the
//! decoder is present. Both clips are 3 s of `testsrc` from ffmpeg, 17 KB each. The MP4 pair
//! and the FLV pair (same encoder settings, muxed into `.flv` instead) cover the two sites of
//! the gate: `decode::try_video_tier`'s by-bytes mini-clip check, and the FLV remux branch at
//! both that site and `streamsrc::mp4_mkv_or_else_tiers` that must check the remux output on
//! its own — `mf` upstream of it is trivially true for a real `.flv` (no mp4/mkv mini-clip to
//! derive it from).
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

/// One 4:4:4-then-4:2:0 pair, run against whatever `mf_undecodable_reason`-checkable bytes
/// the caller hands it (raw mp4/mkv bytes, or an already-remuxed FLV mini-clip) plus the
/// original file bytes to probe end to end through `core::probe_cover`.
fn assert_444_refused_and_420_decodes(
    reason_bytes_444: &[u8],
    probe_bytes_444: &[u8],
    reason_bytes_420: &[u8],
    probe_bytes_420: &[u8],
) {
    let reason = core::vcodec::mf_undecodable_reason(&mut std::io::Cursor::new(reason_bytes_444))
        .expect("the fixture must be identified as High 4:4:4 Predictive");
    assert!(reason.contains("244"), "{reason}");

    let grabs_before = core::video::mf_grab_attempts();
    let started = Instant::now();
    let probed = core::probe_cover(probe_bytes_444);
    let took = started.elapsed();
    assert_eq!(probed, None, "a 4:4:4 clip must yield no frame");
    assert_eq!(
        core::video::mf_grab_attempts(),
        grabs_before,
        "Media Foundation must not have been asked at all (issue #35)"
    );
    assert!(took < Duration::from_secs(2), "the refusal took {took:?}");

    // Same encoder, same size, decodable profile. Only where the OS actually has the
    // decoder (a Server image may not), and it must go THROUGH MF.
    assert_eq!(
        core::vcodec::mf_undecodable_reason(&mut std::io::Cursor::new(reason_bytes_420)),
        None,
        "High 8-bit 4:2:0 must not be refused"
    );
    let decoder_present = core::video::media_foundation_available()
        && core::vcodec::identify(&mut std::io::Cursor::new(reason_bytes_420))
            .and_then(|info| info.subtype)
            .and_then(core::vcodec::decoder_installed)
            == Some(true);
    if !decoder_present {
        eprintln!("no H.264 decoder on this Windows - the decode half is skipped");
        return;
    }
    let grabs_before = core::video::mf_grab_attempts();
    let dims = core::probe_cover(probe_bytes_420);
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

#[test]
fn h264_444_is_refused_before_media_foundation_is_asked_and_420_still_decodes() {
    // Phase 1-2: the MP4 pair. `mf_undecodable_reason` reads the container's own
    // avcC/CodecPrivate directly, so the raw file bytes ARE the reason-check bytes.
    let bytes_444 = fixture("h264-high444-320x240.mp4");
    let bytes_420 = fixture("h264-high-320x240.mp4");
    assert_444_refused_and_420_decodes(&bytes_444, &bytes_444, &bytes_420, &bytes_420);

    // Phase 3-4: the FLV twin (issue for this fix — the FLV remux branch in
    // `decode::try_video_tier` and `streamsrc::mp4_mkv_or_else_tiers` must compute
    // `mf_undecodable_reason` on the remux output, since `mf` upstream of it is
    // trivially true for a real `.flv`). `mf_undecodable_reason` cannot read FLV's own
    // tag layout, so the reason-check bytes here are the remuxed mini-MP4, not the raw
    // file — proving the fixture AND the bridge `flv::keyframe_mini_mp4` builds in one call.
    let flv_444 = fixture("h264-high444-320x240.flv");
    let flv_420 = fixture("h264-high-320x240.flv");
    assert_eq!(
        core::vcodec::mf_undecodable_reason(&mut std::io::Cursor::new(&flv_444)),
        None,
        "raw FLV bytes never parse as mp4/mkv, so the un-remuxed gate must stay silent"
    );
    let flv_444_clip = core::flv::keyframe_mini_mp4(&mut std::io::Cursor::new(&flv_444))
        .expect("the 4:4:4 FLV fixture must remux to a mini-MP4");
    let flv_420_clip = core::flv::keyframe_mini_mp4(&mut std::io::Cursor::new(&flv_420))
        .expect("the 4:2:0 FLV fixture must remux to a mini-MP4");
    assert_444_refused_and_420_decodes(&flv_444_clip, &flv_444, &flv_420_clip, &flv_420);
}
