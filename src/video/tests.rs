#![cfg(test)]

use super::is_near_black;

/// Build an RGBA buffer of `n` pixels, every channel set to `v`.
fn flat(n: usize, v: u8) -> Vec<u8> {
    let mut b = Vec::with_capacity(n * 4);
    for _ in 0..n {
        b.extend_from_slice(&[v, v, v, 255]);
    }
    b
}

/// THE case this exists for: an XviD packed-bitstream N-VOP placeholder decodes to a real
/// buffer with no picture in it. Accepting it is how issue #26 got an all-black thumbnail
/// while the user's offset setting was working perfectly.
#[test]
fn an_empty_frame_reads_as_black() {
    assert!(is_near_black(&flat(4000, 0)));
    // Studio-range black sits near 16, not 0, and carries a little decode noise.
    assert!(is_near_black(&flat(4000, 16)));
}

/// A real picture must NEVER be discarded as black, or a legitimate dark frame would be
/// skipped and the thumbnail would come from somewhere the user did not ask for.
#[test]
fn a_real_picture_is_not_black() {
    assert!(!is_near_black(&flat(4000, 200)));
    // Just past the threshold, uniformly — the tightest true negative.
    assert!(!is_near_black(&flat(4000, 19)));
}

/// A frame that is black almost everywhere but has real content somewhere is a PICTURE
/// (letterboxed, a fade-in with a logo, a dark scene). The sampler must find it, whichever
/// row it lands on — so probe every offset a single bright pixel could occupy.
#[test]
fn one_bright_region_anywhere_defeats_the_black_verdict() {
    for offset in 0..400usize {
        let mut b = flat(400, 0);
        // Wrap rather than clamp, so the run is genuinely 120px wide at EVERY offset —
        // clamping shortened it near the end and tested the harness, not the code.
        for k in 0..120usize {
            b[((offset + k) % 400) * 4] = 240;
        }
        assert!(
            !is_near_black(&b),
            "a 120px bright run at offset {offset} was called black"
        );
    }
    // And the tightest possible case: ONE bright pixel, at every position. A strided
    // sampler fails this; a full scan cannot.
    for offset in 0..400usize {
        let mut b = flat(400, 0);
        b[offset * 4 + 1] = 240; // green channel, to prove it is not just red that counts
        assert!(
            !is_near_black(&b),
            "a single bright pixel at offset {offset} was called black"
        );
    }
}

/// Degenerate buffers must not be called black: "black" makes us SKIP a frame, so a wrong
/// yes throws away the only picture we had. An empty buffer is unknown, not black.
#[test]
fn a_buffer_too_small_to_judge_is_not_called_black() {
    assert!(!is_near_black(&[]));
    assert!(!is_near_black(&[0, 0]));
}

/// The block-caching stream must let Media Foundation decode a representative frame from
/// containers we have no bespoke index parser for (AVI, WMV). Runs against the corpus
/// samples `scripts\build-corpus.ps1` downloads (`sample.avi`/`sample.wmv`); skips
/// wherever the corpus isn't present (e.g. CI, or before that script has run).
#[test]
fn block_stream_decodes_avi_and_wmv() {
    let dirs: Vec<_> = [crate::testcorpus::real_dir(), crate::testcorpus::dir()]
        .into_iter()
        .filter(|p| p.exists())
        .collect();
    let samples: Vec<_> = ["sample.avi", "sample.wmv"]
        .into_iter()
        .filter_map(|name| dirs.iter().map(|d| d.join(name)).find(|p| p.is_file()))
        .collect();
    if dirs.is_empty() {
        eprintln!("block_stream_decodes_avi_and_wmv: no test corpus present — skipping");
        return;
    }
    let mut tested = 0;
    for path in &samples {
        tested += 1;
        let path = path.to_str().expect("corpus path is valid UTF-8");
        let frame = super::frame_from_block_stream_file(path, 0.30)
            .unwrap_or_else(|| panic!("block stream failed to decode {path}"));
        assert!(frame.width() > 0 && frame.height() > 0);
        eprintln!(
            "block_stream: {path} → {}x{}",
            frame.width(),
            frame.height()
        );
    }
    if tested == 0 {
        eprintln!("block_stream_decodes_avi_and_wmv: no avi/wmv samples in corpus — skipping");
    }
}

/// A CR3 is ISO-BMFF (shares the `ftyp` box with HEIC/AVIF/MP4), so without this
/// exclusion it gets routed into the video cascade, every MF tier fails to demux a RAW
/// photo, and `streamsrc.rs` returns `E_FAIL` directly with no fall-through to the
/// RAW/WIC cascade that already recognizes `crx `/`cr3 ` (`rawsniff.rs`).
#[test]
fn cr3_and_crx_ftyp_brands_are_not_video() {
    let head_for = |brand: &[u8; 4]| -> Vec<u8> {
        let mut h = vec![0u8; 12];
        h[4..8].copy_from_slice(b"ftyp");
        h[8..12].copy_from_slice(brand);
        h
    };
    assert!(!super::is_video_magic(&head_for(b"crx ")));
    assert!(!super::is_video_magic(&head_for(b"cr3 ")));
    // A real MP4 brand must still be treated as video — the exclusion list must stay narrow.
    assert!(super::is_video_magic(&head_for(b"isom")));
}

/// The bounded worker must hand back a fast `f`'s answer, and must come back EMPTY, on
/// time, for an `f` that overruns - leaving that worker to finish on its own and keeping
/// it on the strand ledger until it does. Both halves are the issue #35 contract: the
/// calling (shell) thread is always free again after the budget, whatever Media
/// Foundation is doing on the worker.
///
/// Timing discipline inherited from the watchdog test this replaces: the FAST half gets
/// a ten-second budget for an instant `f`, so only a genuinely broken wait can miss the
/// answer (a tight budget there is a scheduling test, and it flaked exactly once under a
/// saturated `cargo mutants` run). The SLOW half blocks `f` on a flag we release, and then
/// WAITS for the ledger to clear rather than sleeping a fixed time, so a loaded box can
/// only make it slower, never wrong.
#[test]
fn bounded_worker_returns_on_time_and_records_the_strand() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    assert_eq!(
        super::run_bounded_pumping(Duration::from_secs(10), "test-fast", || Some(42)),
        Some(42),
        "a worker that finishes inside the budget must hand back its answer"
    );

    let release = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let before = super::stranded_workers();
    let started = Instant::now();
    let r = super::run_bounded_pumping(Duration::from_millis(30), "test-slow", move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !gate.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        Some(7)
    });
    assert_eq!(
        r, None,
        "an overrunning worker must be given up on, not waited for"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the caller came back late: {:?}",
        started.elapsed()
    );
    assert!(
        super::stranded_workers() > before,
        "the overrun must be on the strand ledger while the worker still runs"
    );
    // Slow is not stuck: inside the grace period the host is NOT wedged...
    assert!(!super::mf_wedged());
    // ...and would be, were this same strand still running past the grace.
    let later = Instant::now() + super::STRAND_GRACE + Duration::from_secs(1);
    assert!(super::wedged_at(later));

    release.store(true, Ordering::SeqCst);
    // The worker flips its own `done` as its last act; wait for that, never sleep for it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while super::stranded_workers() > before && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        super::stranded_workers(),
        before,
        "a worker that finishes must leave the ledger on its own"
    );
    assert!(
        !super::wedged_at(later),
        "a finished worker must not count as a wedge however old its strand is"
    );
}

/// `frame_from_bytes` is now a thin `.to_vec()` + delegate over
/// [`super::frame_from_owned_bytes`] (the A202 fix: callers that already own a `Vec<u8>`
/// — every mp4/mkv/flv remux buffer, `mp4_remux_moov`'s output — call the owned entry
/// point directly instead of paying for a second copy). Both entry points must still
/// agree on the same input regardless of which one a caller reaches for; empty bytes can
/// never produce a keyframe on any host, Media Foundation present or not, so this holds
/// without needing the video corpus.
#[test]
fn frame_from_bytes_and_frame_from_owned_bytes_agree_on_the_same_input() {
    assert!(super::frame_from_bytes(&[]).is_none());
    assert!(super::frame_from_owned_bytes(Vec::new()).is_none());
}
