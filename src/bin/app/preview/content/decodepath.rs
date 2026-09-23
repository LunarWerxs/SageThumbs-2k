//! How one file is decoded: streamed or read, budgeted, plus the Markdown-image and PDF workers.

use super::*;

/// Markdown remote-image fetch cap: badges are a few KB, hotlinked art rarely tops 8 MB.
pub(super) const MD_IMG_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Per-phase network timeout for one markdown image (seconds).
pub(super) const MD_IMG_TIMEOUT_SECS: u64 = 8;

/// Fetch + decode one REMOTE markdown image on a worker thread (opt-in toggle path).
/// HTTPS-only + byte-capped via `http_fetch_capped`; decode is budget-bounded; the result
/// posts back as `WM_APP_MDIMG` with `Box<(gen, src, Option<DecodedRgba>)>` (a stale `gen`
/// is dropped by the handler). The UI thread never blocks.
pub(in super::super) unsafe fn spawn_md_img(hwnd: HWND, src: String, gen: u64) {
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let hwnd = HWND(hwnd_raw as *mut c_void);
        let decoded =
            crate::sponsors::http_fetch_capped(&src, false, MD_IMG_MAX_BYTES, MD_IMG_TIMEOUT_SECS)
                .and_then(|b| decode_preview_budgeted(std::sync::Arc::new(b)))
                .map(|img| {
                    // Same display-cap policy as local markdown images (bounds the cached DIB).
                    // `reduce_to_fit` never enlarges, so it carries its own no-op case.
                    let img = sagethumbs2k_core::decode::reduce_to_fit(img, 2048, 4096);
                    rgba8_full(img)
                });
        let payload: Box<(u64, String, Option<DecodedRgba>)> = Box::new((gen, src, decoded));
        post_boxed(hwnd, super::super::window::WM_APP_MDIMG, gen, payload);
    });
}

/// Decode PDF `page` (0-based) via the OS renderer + fetch the page count, posting the count
/// (`WM_APP_PDFINFO`) and then the page image (`WM_APP_RENDER`, reusing the normal install path).
pub(in super::super) unsafe fn spawn_decode_pdf(hwnd: HWND, path: String, page: u32, gen: u64) {
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let hwnd = HWND(hwnd_raw as *mut c_void);
        // Page-turn key held down: the OS rasteriser is the expensive part, so bail before it
        // rather than render a page nobody is on any more.
        if abandoned_logged(gen, "PDF page render") {
            return;
        }
        let rendered = sagethumbs2k_core::pdf::render_page_counted_path(&path, page, 1600);
        let (rgba, count) = match rendered {
            Some((png, count)) => {
                let d = image::load_from_memory(&png).ok().map(rgba8_full);
                (d, Some(count))
            }
            None => (None, None),
        };
        if let Some(c) = count {
            post_boxed(hwnd, WM_APP_PDFINFO, gen, Box::new((gen, c)));
        }
        // Same post-and-reclaim as every other decode result: the box is handed to the UI
        // thread, or freed here if the window died before the post.
        unsafe { post_render(hwnd, gen, rgba.map(std::sync::Arc::new)) };
    });
}

/// Read the file and run the budgeted decoder, converting the result to tight RGBA8.
pub(super) fn read_and_decode(path: &str) -> Option<DecodedRgba> {
    // Formats that stream + downscale off the file handle (OpenEXR) never go
    // through the bounded whole-file read — a 12K render pass is past every cap.
    if let Some(img) = streamed_decode(path) {
        return Some(img);
    }
    let bytes = sagethumbs2k_core::decode::read_preview_capped(path).ok()?;
    decode_loaded(std::sync::Arc::new(bytes))
}

/// The by-path streaming decode (see `decode::decode_preview_streamed`), converted
/// to tight RGBA8. `None` when the path isn't one of those formats.
pub(super) fn streamed_decode(path: &str) -> Option<DecodedRgba> {
    // A Photoshop document keeps the viewer's own two stages - its baked preview at once,
    // then the stored composite at `STORED_COMPOSITE_EDGE` - rather than one streamed pass
    // capped at the thumbnail edge, which would also end the sharpen chase before it began.
    if is_photoshop(path) {
        return None;
    }
    use sagethumbs2k_core::decode;
    // A file past the input ceiling is read as large as the viewer would show it whole; the
    // streamed EXR/XCF decode keeps its own, smaller edge (its cost grows with the edge).
    let img = decode::decode_streamed_format(path, decode::EXR_PATH_EDGE)
        .or_else(|| decode::decode_oversized_path(path, decode::OVERSIZED_VIEW_EDGE))?;
    Some(rgba8_full(img))
}

/// Decode bytes already acquired by the path-aware reader. Keeping this separate lets the
/// animation probe fall through without issuing a second file read for ordinary PNG/WebP/GIF.
///
/// Takes the buffer as an `Arc` because the caller ALSO hands it to the sharpen pass. It used
/// to be a `Vec` and the caller cloned it, which is a full copy of the file on every preview:
/// invisible for a 2 MB JPEG, 120 MB for a big PNG.
pub(super) fn decode_loaded(bytes: std::sync::Arc<Vec<u8>>) -> Option<DecodedRgba> {
    let img = decode_preview_budgeted(bytes)?;
    Some(rgba8_full(img))
}

/// Run `decode::decode_preview` on a detached sub-thread, returning its result only if it
/// finishes within [`sagethumbs2k_core::safety::PREVIEW_DECODE_BUDGET`]. On timeout returns
/// `None` and abandons the
/// sub-thread (it sends into a dropped channel and exits on its own). The sub-thread holds
/// a COM MTA apartment because the WIC decode tier (HEIC/RAW/JPEG-XR) needs it — the
/// detach/timeout shape is verbatim from `previewhandler::decode_preview_budgeted`, minus the
/// DLL `ModuleRef` pin (this is an EXE, not the shell-loaded DLL).
///
/// **Deliberately NOT `decode_preview_capped`, unlike the preview-pane version.** This is the
/// one decode this whole viewer uses for zoom-to-detail (`spawn_decode_full`), the headless
/// `--shot` capture (`decode_sync`), and the `--bench-preview` "cold, full decode" measurement
/// — all three need the real resolution, and `display_scaled_first_paint`'s doc comment
/// depends on this call always being a full decode ("zoom still has full resolution behind
/// it... byte-for-byte what it was before"). Capping it to the pane's small target edge would
/// fix the same 12s-budget risk previewhandler's issue #11 fix addressed, but it would also
/// silently cap every zoom and screenshot in the viewer — that needs a separate, smaller-edge
/// decode path for the background-prefetch case specifically, not a blanket cap here.
///
/// **Holds a [`safety::AbandonTicket`] for the whole worker lifetime (audit E02, 2026-09-07):**
/// before this fix, a worker that outlived `PREVIEW_DECODE_BUDGET` was simply forgotten by this
/// function on timeout: it kept running and pinning a thread, but never counted against
/// `safety::abandoned_workers()`/`MAX_ABANDONED_WORKERS`, unlike the budgeted detached-worker paths
/// in the process (`spawn_budgeted`, the menu-preview decode). `decode_preview(&bytes)` is
/// in-memory and CPU-bound, not I/O, so what can outlive the budget here is a decode that never
/// returns; repeated cases of that could grow the viewer's thread count past the documented cap
/// with nothing to show for it. The ticket closes that gap: `caller_gave_up` on timeout,
/// `worker_finished` when the worker actually returns, exactly the handshake `spawn_budgeted`
/// itself uses.
pub(super) fn decode_preview_budgeted(
    bytes: std::sync::Arc<Vec<u8>>,
) -> Option<image::DynamicImage> {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
    // The same pre-spawn gate `spawn_budgeted` consults: past the process-wide abandoned budget
    // this must not start one more worker, or the count it maintains could never throttle it.
    if sagethumbs2k_core::safety::abandoned_budget_exhausted() {
        return None;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let ticket = sagethumbs2k_core::safety::AbandonTicket::new();
    let worker_ticket = ticket.clone();
    std::thread::spawn(move || {
        let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let out = sagethumbs2k_core::decode::decode_preview(&bytes).ok();
        if inited {
            unsafe { CoUninitialize() };
        }
        let _ = tx.send(out);
        worker_ticket.worker_finished();
    });
    match rx.recv_timeout(sagethumbs2k_core::safety::PREVIEW_DECODE_BUDGET) {
        Ok(out) => out,
        Err(_) => {
            ticket.caller_gave_up();
            None
        }
    }
}

/// Does `path` start with Photoshop's `8BPS` signature? Four bytes, never the file.
fn is_photoshop(path: &str) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok_and(|()| &magic == b"8BPS")
}
