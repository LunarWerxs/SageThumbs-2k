//! The decode workers: spawn, decode, post the result back to the window.

use super::*;

/// Formats that stream + downscale off the file handle (OpenEXR) skip the read entirely — a
/// 12K render pass is past every in-memory cap. Never animated, so this can post the
/// single-frame result straight away. Returns `true` when it did.
pub(super) unsafe fn try_post_streamed(hwnd: HWND, gen: u64, path: &str) -> bool {
    let Some(decoded) = streamed_decode(path) else {
        return false;
    };
    post_render(hwnd, gen, Some(std::sync::Arc::new(decoded)));
    true
}

/// CODEC-SCALED DECODE, ahead of the read, for a non-animated format: when this succeeds
/// nothing else needs the file's bytes at all, so reading them first would be pure waste.
/// …and STOPS there. The full decode is deferred until something actually needs those pixels,
/// which for a fit view is never: the pane shows about a megapixel and this holds up to 2048
/// on the long edge. Measured, 12 MP JPEG: 59 ms here against 250 ms for the full decode, so
/// an arrow step stops paying the 250 ms at all.
///
/// What makes deferring SAFE rather than a downgrade is `DecodedRgba::nat`: the pixels are
/// small but the render still reports the real image size, so the caption, the window sizing
/// and "100%" are unchanged. `window::ensure_full_for_zoom` fetches the real thing the moment
/// a zoom asks for more detail than this holds.
///
/// Only reached for codecs that genuinely decode small (JPEG's DCT reduction); anything else
/// returns `false` and the caller falls through to the full decode unchanged.
pub(super) unsafe fn try_post_quick_first_paint(hwnd: HWND, gen: u64, path: &str) -> bool {
    let Some(quick) = display_scaled_first_paint(path) else {
        return false;
    };
    let quick = std::sync::Arc::new(quick);
    cache_put(path, std::sync::Arc::clone(&quick));
    post_render(hwnd, gen, Some(quick));
    true
}

/// Box `payload` into the message's `LPARAM`, reclaiming it if the window has already gone.
pub(super) unsafe fn post_boxed<T>(hwnd: HWND, msg: u32, gen: u64, payload: Box<T>) {
    let raw = Box::into_raw(payload);
    if PostMessageW(Some(hwnd), msg, WPARAM(gen as usize), LPARAM(raw as isize)).is_err() {
        drop(Box::from_raw(raw));
    }
}

/// Post an animated frame list as `WM_APP_ANIM`.
pub(super) unsafe fn post_anim(hwnd: HWND, gen: u64, frames: Vec<(DecodedRgba, u32)>) {
    post_boxed(hwnd, WM_APP_ANIM, gen, Box::new((gen, frames)));
}

/// Animated GIF/APNG/animated-WebP → post the whole frame list. A static file of the same
/// extension has no frames here and falls through to the single-frame path. Returns `true`
/// when a frame list was posted.
pub(super) unsafe fn try_post_animation(
    hwnd: HWND,
    gen: u64,
    bytes: Option<&[u8]>,
    ext: &str,
) -> bool {
    let Some(bytes) = bytes else {
        return false;
    };
    let Some(frames) = super::super::anim::decode_animation(bytes, ext) else {
        return false;
    };
    post_anim(hwnd, gen, frames);
    true
}

/// Decode `bytes` as a static image, cache + post it, and return the shown `(w, h)` for the
/// PSD/PSB sharpen chase below, or `None` if the decode failed.
pub(super) unsafe fn decode_and_post_static(
    hwnd: HWND,
    gen: u64,
    path: &str,
    bytes: Option<std::sync::Arc<Vec<u8>>>,
) -> Option<(i32, i32)> {
    let stage_start = std::time::Instant::now();
    let decoded = bytes.and_then(decode_loaded).map(std::sync::Arc::new);
    if let Some(line) = st2k_base::safety::stage_stall_report(
        "decode",
        stage_start.elapsed(),
        st2k_base::safety::PREVIEW_DECODE_BUDGET,
        gen,
        path,
    ) {
        st2k_base::safety::log_debug(&line);
    }
    // Cache and hand over the SAME allocation — one decode, no copy of the pixels.
    let shown = decoded.as_ref().map(|d| (d.w, d.h));
    if let Some(d) = &decoded {
        cache_put(path, std::sync::Arc::clone(d));
    }
    post_render(hwnd, gen, decoded);
    shown
}

/// The start of every decode worker: the HWND rebuilt from its raw value (HWND isn't `Send`),
/// and a COM apartment held for the rest of the closure. ISSUE #33: the workers reach WIC by
/// way of `read_preview_capped` (the oversized rescue decodes THROUGH WIC by path) and the WIC
/// tier of the in-memory decode, and without an apartment every one of those calls answered
/// `CoInitialize has not been called (0x800401F0)` and fell back to the slow tier, or to
/// nothing; a repeat MTA init inside is a no-op. `None` when the load was already superseded
/// (a held-down arrow key): nothing read or decoded yet, so one atomic load reclaims the worker.
fn begin_decode_worker(
    hwnd_raw: isize,
    gen: u64,
    what: &str,
) -> Option<(HWND, Option<st2k_base::parallel::ComGuard>)> {
    let hwnd = HWND(hwnd_raw as *mut c_void);
    let com = st2k_base::parallel::ComGuard::mta();
    (!abandoned_logged(gen, what)).then_some((hwnd, com))
}

/// Kick off an async decode of `path` on a detached worker thread. The result (or `None`
/// on failure/timeout) is posted back to `hwnd` as `WM_APP_RENDER` carrying a boxed
/// `(gen, Option<SharedRgba>)`; `gen` lets the UI thread drop a stale result after the
/// user has already switched files. The UI thread NEVER blocks on the decode.
pub(in super::super) unsafe fn spawn_decode(hwnd: HWND, path: String, gen: u64) {
    // Cache hit: answer on the spot, no thread, no read, no decode. Stepping ←/→ through a
    // folder revisits the same files constantly, and this is what makes that feel instant.
    if let Some(hit) = cache_get(&path) {
        post_render(hwnd, gen, Some(hit));
        return;
    }
    begin_generation(gen);
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let Some((hwnd, _com)) = begin_decode_worker(hwnd_raw, gen, "decode") else {
            return;
        };
        if try_post_streamed(hwnd, gen, &path) {
            return;
        }
        let ext = lower_ext(&path);
        // The animated extensions are excluded from the quick codec-scaled path rather than
        // ordered around it. They need the bytes for the frame probe below regardless, and
        // none of them is a codec that decodes small, so nothing is given up. That exclusion
        // also preserves the rule this path was built under: an animated file posts
        // `WM_APP_ANIM` and returns, and posting a still render as well left the window with
        // two render paths half-applied, which crashed outright on a 24 MP PNG (access
        // violation, found by `--bench-nav` at exactly the step that reaches it). Keeping the
        // two mutually exclusive is what makes that unrepresentable.
        let animated_ext = matches!(ext.as_str(), "gif" | "png" | "apng" | "webp");
        if !animated_ext && try_post_quick_first_paint(hwnd, gen, &path) {
            return;
        }
        // One bounded/path-aware read for BOTH the animation probe and static fallback.
        // This also gives the standalone viewer the core's PSD/PSB/Blender head-preview and
        // oversized streamed-cover fast paths instead of blindly buffering the whole file.
        // Shared, never copied: the decode moves it into its worker and the sharpen pass needs
        // the same buffer afterwards. Cloning it instead cost a full copy of the file per
        // preview (measured: ~120 MB on a 24 MP PNG, for nothing).
        let bytes = st2k_codecs::decode::read_preview_capped(&path)
            .ok()
            .map(std::sync::Arc::new);
        if animated_ext && try_post_animation(hwnd, gen, bytes.as_deref().map(Vec::as_slice), &ext)
        {
            return;
        }
        let shown = decode_and_post_static(hwnd, gen, &path, bytes.clone());
        // The fast preview is now on screen. For PSD/PSB that preview is Photoshop's small
        // baked-in thumbnail, so chase it with the real composite and post a SECOND result.
        // Two-stage on purpose: the composite can take seconds, and paying that up front
        // would trade an instant preview for a long blank window.
        if let Some((bytes, shown)) = sharpen_start(&path, bytes, shown) {
            spawn_sharpen(hwnd, path, bytes, shown, gen);
        }
    });
}

/// The file's bytes as the first stage read them, shared with the sharpen pass.
type SharedBytes = std::sync::Arc<Vec<u8>>;

/// Where the composite chase starts from: the bytes the first stage read and the size it drew,
/// or `None` when there is nothing to chase.
///
/// A Photoshop document saved without its baked preview ("Image Previews: Never Save") draws
/// nothing at the first stage, and is the one that most needs the composite, so it is chased
/// from nothing rather than left on the card (issue #46). Past the input ceiling the preview
/// read declines such a document outright, so its header is read on its own: it is all the
/// chase needs.
fn sharpen_start(
    path: &str,
    bytes: Option<SharedBytes>,
    shown: Option<(i32, i32)>,
) -> Option<(SharedBytes, (i32, i32))> {
    let bytes = bytes.or_else(|| photoshop_header(path).map(std::sync::Arc::new))?;
    let shown = shown.or_else(|| bytes.starts_with(b"8BPS").then_some((0, 0)))?;
    Some((bytes, shown))
}

/// Decode `path` at FULL resolution and post it, skipping the codec-scaled shortcut.
///
/// The other half of the deferral in [`spawn_decode`]: the fit view is served by display-sized
/// pixels, and this is what fetches the real ones once a zoom asks for detail they do not hold.
/// Deliberately a separate entry point rather than a flag — a caller that wants full resolution
/// wants it unconditionally, and threading a "no really, all of it" boolean through the normal
/// path is how the shortcut would eventually get taken by accident.
pub(in super::super) unsafe fn spawn_decode_full(hwnd: HWND, path: String, gen: u64) {
    if let Some(hit) = cache_get(&path).filter(|d| d.is_full()) {
        post_render(hwnd, gen, Some(hit));
        return;
    }
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        // Zoomed, then navigated away before this got a slice of CPU: nothing to do.
        let Some((hwnd, _com)) = begin_decode_worker(hwnd_raw, gen, "full-resolution decode")
        else {
            return;
        };
        let decoded = read_and_decode(&path).map(std::sync::Arc::new);
        if let Some(d) = &decoded {
            // Replaces the scaled entry under the same key, so a later revisit gets the full
            // pixels straight away rather than re-deciding.
            cache_put(&path, std::sync::Arc::clone(d));
        }
        post_render(hwnd, gen, decoded);
    });
}

/// Post a finished decode to the UI thread, reclaiming the box if the window has already gone.
pub(super) unsafe fn post_render(hwnd: HWND, gen: u64, decoded: Option<SharedRgba>) {
    post_boxed(hwnd, WM_APP_RENDER, gen, Box::new((gen, decoded)));
}

/// Decode the full composite on its own worker and post it as a second `WM_APP_RENDER`.
///
/// Reuses `gen`, so if the user has already arrowed on, the upgrade is dropped by the exact
/// same staleness check that guards the first result — no new state, no new message. COM is
/// initialised here because `decode_full` can land on the WIC tier, which needs an apartment.
pub(super) unsafe fn spawn_sharpen(
    hwnd: HWND,
    path: String,
    bytes: std::sync::Arc<Vec<u8>>,
    shown: (i32, i32),
    gen: u64,
) {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let hwnd = HWND(hwnd_raw as *mut c_void);
        // Worth the most of any of these checks: the composite shells out to ImageMagick and
        // can take SECONDS. Running one to completion for a document the user has already
        // arrowed past is the single largest piece of wasted work the viewer could do.
        if abandoned_logged(gen, "sharpen composite") {
            return;
        }
        let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let sharp = sharper_composite(&path, &bytes, shown);
        if inited {
            unsafe { CoUninitialize() };
        }
        if let Some(sharp) = sharp {
            let arc = std::sync::Arc::new(sharp);
            // Cache the SHARP one: arrowing back must not drop to the small preview again.
            cache_put(&path, std::sync::Arc::clone(&arc));
            unsafe { post_render(hwnd, gen, Some(arc)) };
        }
    });
}
