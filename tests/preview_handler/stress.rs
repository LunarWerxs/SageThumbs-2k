//! The preview pane under load: a thumbnail storm beside it, and a 76-megapixel JP2 inside the budget.

use super::*;

/// A folder of DISTINCT jp2 files, so nothing can be served from a cache: `count` copies
/// of `jp2`, each padded with a different amount of trailing slack so no two files are
/// byte-identical. Returns the folder and the paths written into it.
pub(super) fn write_distinct_jp2_folder(
    jp2: &[u8],
    count: usize,
) -> (std::path::PathBuf, Vec<std::path::PathBuf>) {
    let dir = std::env::temp_dir().join(format!("st2k_jp2_folder_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let paths: Vec<std::path::PathBuf> = (0..count)
        .map(|i| {
            let p = dir.join(format!("photo_{i:02}.jp2"));
            // Append i bytes of trailing slack so each file is byte-distinct.
            let mut bytes = jp2.to_vec();
            bytes.extend(std::iter::repeat_n(0u8, i));
            std::fs::write(&p, &bytes).unwrap();
            p
        })
        .collect();
    (dir, paths)
}

/// The running "thumbnail load storm" background threads and their per-lane progress
/// counters. One counter PER LANE (rather than one shared total) so readiness can be
/// checked per lane — a shared total could clear its floor from three fast lanes while a
/// fourth had not started at all.
pub(super) struct ThumbnailStorm {
    pub(super) stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub(super) lane_calls: Vec<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    pub(super) handles: Vec<std::thread::JoinHandle<()>>,
}

/// One storm lane: repeatedly build a fresh thumbnail provider over the next file in
/// `paths` and fetch its thumbnail, counting completed round trips, until `stop` is set.
pub(super) unsafe fn run_thumbnail_storm_lane(
    lane: usize,
    stop: &std::sync::atomic::AtomicBool,
    calls: &std::sync::atomic::AtomicUsize,
    paths: &[std::path::PathBuf],
) {
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    let mut i = lane;
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        let bytes = std::fs::read(&paths[i % paths.len()]).unwrap();
        i += 1;
        let Ok(init) = create_thumb_provider() else {
            continue;
        };
        let Ok(stream) = SHCreateMemStream(Some(&bytes)).ok_or(Error::from(E_FAIL)) else {
            continue;
        };
        if init.Initialize(&stream, 0).is_err() {
            continue;
        }
        let Ok(provider) = init.cast::<IThumbnailProvider>() else {
            continue;
        };
        let mut hbmp = windows::Win32::Graphics::Gdi::HBITMAP::default();
        let mut alpha = WTS_ALPHATYPE::default();
        if provider.GetThumbnail(256, &mut hbmp, &mut alpha).is_ok() && !hbmp.is_invalid() {
            let _ = DeleteObject(hbmp.into());
        }
        calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Spawns `lanes` background threads that keep hammering `GetThumbnail` over `paths`
/// until told to stop — Explorer's thumbnail pass over the neighbouring files, running
/// for as long as the pane test needs it to.
pub(super) fn spawn_thumbnail_storm(
    paths: Vec<std::path::PathBuf>,
    lanes: usize,
) -> ThumbnailStorm {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let lane_calls: Vec<std::sync::Arc<std::sync::atomic::AtomicUsize>> = (0..lanes)
        .map(|_| std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)))
        .collect();
    let mut handles = Vec::new();
    for (lane, calls) in lane_calls.iter().enumerate() {
        let stop = stop.clone();
        let calls = calls.clone();
        let paths = paths.clone();
        handles.push(std::thread::spawn(move || unsafe {
            run_thumbnail_storm_lane(lane, &stop, &calls, &paths)
        }));
    }
    ThumbnailStorm {
        stop,
        lane_calls,
        handles,
    }
}

/// Polls each lane's own counter until it has completed at least `min_calls_per_lane`
/// full `GetThumbnail` round trips, rather than hoping a fixed sleep was long enough. A
/// deadline still bounds the wait so a genuinely stuck lane fails the test instead of
/// hanging.
pub(super) fn wait_for_storm_readiness(
    lane_calls: &[std::sync::Arc<std::sync::atomic::AtomicUsize>],
    min_calls_per_lane: usize,
    deadline: std::time::Instant,
) {
    loop {
        let ready = lane_calls
            .iter()
            .all(|c| c.load(std::sync::atomic::Ordering::Relaxed) >= min_calls_per_lane);
        if ready {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "thumbnail load storm never reached {min_calls_per_lane} call(s) on every lane \
             within the deadline — the storm this test depends on never really started"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Runs the "click through every file" walkthrough against ONE preview handler instance,
/// exactly as the pane does when moving from selection to selection, and returns a
/// description of every selection (if any) that never rendered.
pub(super) unsafe fn run_jp2_pane_walkthrough(paths: &[std::path::PathBuf]) -> Vec<String> {
    let mut blanks: Vec<String> = Vec::new();

    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    ensure_host_class();

    let (tx, rx) = std::sync::mpsc::channel::<isize>();
    let host = std::thread::spawn(move || {
        let parent = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            HOST_CLASS,
            w!(""),
            WS_OVERLAPPED,
            0,
            0,
            PANE_W,
            PANE_H,
            None,
            None,
            None,
            None,
        )
        .expect("host parent window");
        let _ = tx.send(parent.0 as isize);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
    let parent = HWND(rx.recv().unwrap() as *mut c_void);

    // ONE handler, clicked from file to file, exactly as the pane does.
    let init = create_handler().expect("create preview handler");
    let handler: IPreviewHandler = init.cast().expect("QI IPreviewHandler");
    let rect = RECT {
        left: 0,
        top: 0,
        right: PANE_W,
        bottom: PANE_H,
    };
    handler.SetWindow(parent, &rect).expect("SetWindow");

    for p in paths {
        let bytes = std::fs::read(p).unwrap();
        let stream = SHCreateMemStream(Some(&bytes)).expect("SHCreateMemStream");
        init.Initialize(&stream, 0).expect("Initialize");
        handler.DoPreview().expect("DoPreview");
        // The corpus sample's centre pixel is black; anything else means the pane
        // did not end up showing THIS file.
        let px = wait_for_pixel(parent, is_black, 20);
        if !is_black(px) {
            blanks.push(format!(
                "{} -> {px:?}",
                p.file_name().unwrap().to_string_lossy()
            ));
        }
    }

    handler.Unload().expect("Unload");
    drop(handler);
    drop(init);
    let _ = PostMessageW(Some(parent), WM_HOST_CLOSE, WPARAM(0), LPARAM(0));
    let _ = host.join();

    blanks
}

/// Issue #11, the reporter's ACTUAL situation, reproduced locally rather than asked
/// about: a folder holding several jp2 files, the preview pane being clicked from one to
/// the next, while Explorer builds thumbnails for the neighbouring files at the same
/// time. jp2 is decoded by spawning ImageMagick, so every one of those thumbnails is its
/// own subprocess competing with the pane's — the load the single-file test never had.
///
/// Every selection must render. A blank pane here is the bug the reporter is seeing.
#[test]
fn preview_keeps_up_with_a_folder_of_jp2_under_thumbnail_load() {
    let corpus = sagethumbs2k_core::testcorpus::dir().join("sample.jp2");
    let Ok(jp2) = std::fs::read(&corpus) else {
        eprintln!("skipping: ../test-corpus/sample.jp2 not present");
        return;
    };

    const FILES: usize = 12;
    let (dir, paths) = write_distinct_jp2_folder(&jp2, FILES);

    const LANES: usize = 4;
    let storm = spawn_thumbnail_storm(paths.clone(), LANES);

    const MIN_CALLS_PER_LANE: usize = 1;
    let readiness_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    wait_for_storm_readiness(&storm.lane_calls, MIN_CALLS_PER_LANE, readiness_deadline);

    let blanks = unsafe { run_jp2_pane_walkthrough(&paths) };

    storm.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for t in storm.handles {
        let _ = t.join();
    }
    let calls: usize = storm
        .lane_calls
        .iter()
        .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
        .sum();
    let _ = std::fs::remove_dir_all(&dir);

    eprintln!("competing thumbnail decodes during the run: {calls}");
    assert!(
        calls > 0,
        "the thumbnail load never ran — this test proved nothing"
    );
    assert!(
        blanks.is_empty(),
        "{}/{FILES} selections did not render under thumbnail load (issue #11): {blanks:?}",
        blanks.len()
    );
}

/// Issue #11 again, after 1.7.3: a file that is huge in PIXELS rather than bytes.
///
/// The reporter's follow-up was a 9958x7686 (76 MP) map scan from archive.org that is only
/// ~11 MB on disk. It still timed out, because the decode did not scale with what the pane
/// can show: ImageMagick was asked for a fixed 4096 px surface, which was then PNG-encoded
/// (22 MB) and decoded back, and the whole round trip missed the 12 s budget. The pane gave
/// up on a file that decodes fine, which is why "too big" looked like a size problem.
///
/// Decoding now stops at the pane's own target, so this must render, AND it must land well
/// inside the budget rather than scraping past it. `../test-corpus/huge.jp2` comes from
/// `scripts/build-corpus.ps1`; the test skips when the corpus has not been built.
#[test]
fn preview_renders_a_76_megapixel_jp2_inside_the_budget() {
    let path = sagethumbs2k_core::testcorpus::dir().join("huge.jp2");
    let Ok(huge) = std::fs::read(&path) else {
        eprintln!("skipping: ../test-corpus/huge.jp2 not present (run scripts/build-corpus.ps1)");
        return;
    };

    // Sanity: if the sample ever stops being enormous this test proves nothing, so read
    // the dimensions out of the JP2 image header box (`ihdr` = height then width, BE u32).
    let megapixels = huge.windows(4).position(|w| w == b"ihdr").map(|i| {
        let n = |o: usize| u32::from_be_bytes(huge[i + o..i + o + 4].try_into().unwrap()) as u64;
        (n(4) * n(8)) / 1_000_000
    });
    assert!(
        megapixels.is_none_or(|mp| mp > 50),
        "huge.jp2 must be >50 MP to exercise the budget, got {megapixels:?} MP"
    );

    let start = std::time::Instant::now();
    let rendered = unsafe {
        let stream: IStream = SHCreateMemStream(Some(&huge)).expect("SHCreateMemStream");
        // Any non-background pixel proves it painted rather than timing out to empty.
        preview_renders(&stream, |px| !(px[0] > 240 && px[1] > 240 && px[2] > 240))
    };
    let elapsed = start.elapsed();
    assert!(
        rendered,
        "a 76 MP jp2 never rendered in the preview pane after {elapsed:?} (issue #11)"
    );
    // The handler's own budget is 12s. Anything close to that on a warm machine means
    // a loaded machine (Explorer thumbnailing the neighbours) will still miss it.
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "rendered, but took {elapsed:?} — too close to the 12s budget to survive a busy folder"
    );
}
