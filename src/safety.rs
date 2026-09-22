//! Crash-safety boundary.
//!
//! Unwinding a Rust panic across the COM ABI (an `extern "system"`
//! non-unwinding boundary) is undefined behavior, and windows-rs's
//! `#[implement]` macro does NOT wrap method bodies for us. So every COM
//! method funnels through one of these guards.
//!
//! **Important caveat about the release build.** `catch_unwind` only catches
//! *unwinding* panics; with `panic = "abort"` (our release profile, see
//! Cargo.toml) a panic aborts the process *before* any catch — so in release
//! these guards are effectively a debug aid, and the real release behavior is:
//! a panic terminates the host process. The blast radius depends on which
//! coclass panicked:
//!   - **Thumbnail provider** — runs in Explorer's throwaway `dllhost` surrogate,
//!     so an abort there is contained (the surrogate is disposable; Explorer
//!     respawns it). This is the "safe" case the design leans on.
//!   - **Classic context menu / modern `IExplorerCommand`** — these run
//!     **in-process inside `explorer.exe`**, so a panic there aborts the user's
//!     whole shell. Those code paths must therefore be written to *not panic*
//!     (checked indexing, no `unwrap` on attacker-influenced data); the guard is
//!     not a real net for them in release.

use core::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Once, OnceLock};
use std::time::{Duration, Instant};
use windows::core::{Error, Result, HRESULT};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Gdi::{
    CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HBITMAP,
};
use windows_registry::CURRENT_USER;
mod logfile;
mod workers;
pub(crate) use logfile::log_error;
pub use logfile::{debug_logging_on, install_panic_hook, log, log_debug, log_file, os_string};
#[cfg(test)]
use workers::*;
pub use workers::{
    abandoned_budget_exhausted, abandoned_workers, spawn_budgeted, spawn_pinned, start_child_pipes,
    try_spawn, AbandonTicket, Lease, LeasePool, MAX_ABANDONED_WORKERS,
};

/// Longest edge the Explorer preview pane renders at, and the ceiling handed to the decoders
/// on that path. The stream cascade scales to the same value, so the two cannot drift.
pub const PREVIEW_TARGET_EDGE: u32 = 1024;

/// Wall-clock budget for one preview decode, enforced off the host thread (see
/// `previewhandler::decode_preview_budgeted` and the Quick preview's `content.rs`) so a
/// slow decode never freezes the host's message pump. Sized above a typical ImageMagick
/// decode (1-4 s) and well under the ~20 s the host could otherwise be frozen for.
pub const PREVIEW_DECODE_BUDGET: Duration = Duration::from_secs(12);

/// ─── The Quick preview responsiveness contract (audit E02, 2026-09-07) ───
///
/// Slow files must not be able to block the viewer's message pump, and switching away from a
/// slow file must never let its stale output land on screen. That contract is:
///
/// 1. **Show within [`PREVIEW_APPEARANCE_BUDGET`].** `preview::loader::load` shows the window
///    in its `Loading` state before it reads a single byte of the target file (2026-09-05 audit,
///    F10); everything that can block (archive listing, DB/mail markdown, the text/markdown
///    read) runs on a worker thread instead. `tests/preview_async_load.rs` proves this against a
///    4 s slow-read seam and asserts this exact budget as its bound.
/// 2. **No single UI-thread pipeline stage may run past [`PREVIEW_UI_STAGE_BUDGET`]** without
///    it being logged as a stall (`stage_stall_report`, `safety::log_debug`): `apply_resolved`
///    and the render post-back handler (`on_render`) are the two stages this covers. This is a
///    DIAGNOSTIC ceiling, not an enforced one: a stage that legitimately needs longer (e.g.
///    compositing a very large decoded bitmap) still runs to completion, it is just recorded.
/// 3. **A decode worker is abandoned after [`PREVIEW_DECODE_BUDGET`]** (12 s, above) if it has
///    not returned by then; the caller stops waiting and the worker, if it ever finishes, throws
///    its result away. The same budget is used as the "worth logging as slow" threshold for the
///    worker-side prepare/decode stages, since neither has a tighter enforced ceiling of its own.
/// 4. **At most [`MAX_ABANDONED_WORKERS`] abandoned workers may be alive at once.** Past that,
///    [`spawn_budgeted`] and every other [`AbandonTicket`] user refuse to start another one; the
///    Quick preview's `spawn_prepare_load` is one such caller, and past the cap it leaves the window
///    in its `Loading` state (falling through to the fallback card) rather than piling on more
///    blocked threads. Every abandonment that actually counts against this budget is observable:
///    a debug line naming the live count and the cap (rate-limited, since a held arrow key can
///    abandon a worker on every repeat).
///
/// `PREVIEW_UI_STAGE_BUDGET < PREVIEW_APPEARANCE_BUDGET < PREVIEW_DECODE_BUDGET` is an invariant
/// (see `safety::tests::responsiveness_budgets_are_ordered`): the window must appear before a
/// slow decode could possibly finish, and any one UI-thread stage must be far cheaper than the
/// whole appearance budget, or it alone could blow it.
///
/// Wall-clock budget for the viewer to become VISIBLE after `load()` is called on a slow file,
/// measured from process/window-message start (see `tests/preview_async_load.rs`). 1.5 s is
/// generous for a cold/loaded CI box (a real appearance is typically tens of milliseconds, this
/// is showing the `Loading` state, not decoding) while remaining far under the seconds a slow
/// network read or the 12 s decode budget could otherwise stall for.
pub const PREVIEW_APPEARANCE_BUDGET: Duration = Duration::from_millis(1500);

/// Longest a single UI-thread stage of the preview pipeline (`apply_resolved`, the render
/// post-back handler) may take before it is logged as a stall via [`log_debug`]. 100 ms is the
/// commonly-cited threshold past which a UI action stops reading as instantaneous to a human, and
/// every UI-thread stage here is bookkeeping over an already-decoded result (never a decode
/// itself), so it should stay well under it in the overwhelming majority of cases. Exceeding it
/// is diagnostic, not fatal: the stage still runs to completion.
pub const PREVIEW_UI_STAGE_BUDGET: Duration = Duration::from_millis(100);

/// Pure decision + formatting for one pipeline stage's timing, shared by the prepare/decode
/// worker stages and the UI-thread apply/render stages: `Some(line)` (shaped for
/// [`log_debug`]) when `elapsed` exceeded `budget`, `None` when the stage was within budget and
/// nothing should be logged.
///
/// Kept pure (no clock, no I/O) so the log line's SHAPE is unit-tested directly here rather than
/// parsed by any test or script downstream (see `docs/DEVELOPMENT_GOTCHAS.md`, "a string a test
/// parses is an API": this is deliberately not that kind of interface; nothing outside these
/// unit tests should ever match against its text).
pub fn stage_stall_report(
    stage: &str,
    elapsed: Duration,
    budget: Duration,
    generation: u64,
    path: &str,
) -> Option<String> {
    if elapsed <= budget {
        return None;
    }
    Some(format!(
        "preview stage '{stage}' took {}ms (budget {}ms) for generation {generation}: {path}",
        elapsed.as_millis(),
        budget.as_millis(),
    ))
}

/// Wrap a COM method body that returns a raw `HRESULT`.
pub fn guard_hr<F: FnOnce() -> HRESULT>(f: F) -> HRESULT {
    match guard_val(|| Ok::<_, Error>(f())) {
        Ok(hr) => hr,
        Err(e) => e.code(),
    }
}

/// Wrap a COM method body that returns `windows::core::Result<()>`.
pub fn guard<F: FnOnce() -> Result<()>>(f: F) -> Result<()> {
    guard_val(f)
}

/// Wrap a COM method body that returns `windows::core::Result<T>`.
pub fn guard_val<T, F: FnOnce() -> Result<T>>(f: F) -> Result<T> {
    install_panic_hook("dll");
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => {
            log_error("panic crossed a COM boundary -> E_FAIL");
            Err(Error::from(E_FAIL))
        }
    }
}

/// Formats and logs a debug line only when debug logging is on, so the `format!` (and any
/// `Display` work behind it) is skipped entirely on the production path.
#[macro_export]
macro_rules! log_debugf {
    ($($arg:tt)*) => {
        if $crate::safety::debug_logging_on() {
            $crate::safety::log_debug(&format!($($arg)*));
        }
    };
}
pub use crate::log_debugf;

/// Milliseconds since the first logging call in this process — a cheap, monotonic
/// tick that lets lines from one process be ordered without pulling in wall-clock
/// formatting. Truncated to `u64` (decades), so the `<< 1` packing in
/// `safety/logfile.rs` is safe.
pub(crate) fn elapsed_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// True when the OS *app* theme is dark (`AppsUseLightTheme == 0` under
/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize`), defaulting to light
/// (`false`) when the key/value is missing or unreadable.
///
/// A raw, uncached, un-overridden probe — every call re-reads the registry. Shared by
/// `contextmenu::paint::menu_dark` (the classic context-menu preview tile), `previewhandler`'s
/// `theme_default_bg` / `SetBackgroundColor` (the Explorer preview pane) and, via a thin
/// wrapper, the app EXE's `dark::is_dark` (which layers a `ST2K_THEME=light|dark` test
/// override and a process-lifetime `OnceLock` cache on top; that layering is call-site-specific
/// and deliberately NOT duplicated in here, only the raw registry read is shared).
pub fn apps_use_dark_theme() -> bool {
    CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_u32("AppsUseLightTheme"))
        .map(|v| v == 0)
        .unwrap_or(false)
}

/// Validate a straight RGBA buffer for `iw`x`ih` pixels and return its pixel count, or `None`
/// when a dimension is not positive or `rgba` is shorter than `iw * ih` RGBA pixels.
pub fn checked_pixel_count(iw: i32, ih: i32, rgba: &[u8]) -> Option<usize> {
    if iw <= 0 || ih <= 0 {
        return None;
    }
    let px = (iw as usize).checked_mul(ih as usize)?;
    if rgba.len() < px.checked_mul(4)? {
        return None;
    }
    Some(px)
}

/// The `BITMAPINFO` every 32bpp DIB path in this workspace builds: top-down (negative
/// `biHeight`) `BI_RGB`, one plane, 32 bits per pixel. Shared by [`create_dib_section`] here and
/// `contextmenu::paint`'s preview-tile DIB.
///
/// The two windows-rs type names are macro arguments rather than baked in, so each call site's
/// own `use` still names them (they reach the paint code through `contextmenu`'s re-export).
macro_rules! top_down_bmi {
    ($bmi:ty, $hdr:ty, $w:expr, $h:expr) => {{
        let mut bmi = <$bmi>::default();
        bmi.bmiHeader.biSize = core::mem::size_of::<$hdr>() as u32;
        bmi.bmiHeader.biWidth = $w;
        bmi.bmiHeader.biHeight = -$h; // top-down
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = 0; // BI_RGB
        bmi
    }};
}
pub(crate) use top_down_bmi;

/// Create the empty top-down 32bpp `BI_RGB` DIB section `iw`x`ih` the DIB builders in this
/// workspace write their pixels into, returning its bitmap and its (still uninitialised) pixel
/// bytes. The dimensions are trusted to be positive, so validate them first (see
/// [`checked_pixel_count`]).
///
/// # Safety
/// Calls into GDI (`CreateDIBSection`), so this must run with a valid GDI/thread context, and
/// the caller owns the returned `HBITMAP` — it must eventually `DeleteObject` it.
pub unsafe fn create_dib_section(iw: i32, ih: i32) -> Result<(HBITMAP, *mut c_void)> {
    let bmi = top_down_bmi!(BITMAPINFO, BITMAPINFOHEADER, iw, ih);

    let mut bits: *mut c_void = core::ptr::null_mut();
    let hbmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
    if bits.is_null() {
        let _ = DeleteObject(hbmp.into());
        return Err(Error::from(E_FAIL));
    }
    Ok((hbmp, bits))
}

/// Build a top-down 32bpp DIB of `rgba` (straight, non-premultiplied) composited over the
/// opaque `bg` (`COLORREF` 0x00BBGGRR), so painting is a plain `StretchBlt`. `None` on a
/// malformed size / allocation failure — never panics on attacker-controlled dims, which
/// matters for the caller that runs this in-process in `prevhost` under `panic = "abort"`.
///
/// `opaque`: `Some(bool)` when the caller has already worked out whether every pixel is
/// fully opaque (skips the `O(px)` alpha scan below); `None` to have this function work it
/// out itself.
///
/// Shared by the preview-pane host (`previewhandler`'s `WM_PREVIEW_RENDER` arm, in-process
/// in `prevhost`, which passes `opaque: None`) and the Quick preview viewer EXE
/// (`bin/app/preview/content::make_dib`, which passes the opacity it already knows). Homed
/// here, not next to either caller, because `safety` is the one `pub` (crate-external-visible)
/// module this reaches: the app EXE is a SEPARATE crate (its own `[[bin]]`) that can only
/// call `pub` items, and neither `previewhandler` nor the app's own `preview` module is a
/// `pub mod` in `lib.rs`.
///
/// # Safety
/// Calls into GDI (`CreateDIBSection`), so this must run with a valid thread/GDI context, and
/// the caller owns the returned `HBITMAP` — it must eventually `DeleteObject` it, this function
/// does not track its lifetime. There is no other pointer/slice obligation on the caller:
/// `rgba`'s length and `iw`/`ih` are validated (via checked arithmetic) before any raw pointer
/// is touched, and a malformed input returns `None` rather than reading out of bounds.
pub unsafe fn composite_rgba_over_bg(
    iw: i32,
    ih: i32,
    rgba: &[u8],
    bg: u32,
    opaque: Option<bool>,
) -> Option<HBITMAP> {
    let px = checked_pixel_count(iw, ih, rgba)?;
    let (hbmp, bits) = create_dib_section(iw, ih).ok()?;
    let (bg_r, bg_g, bg_b) = (bg & 0xFF, (bg >> 8) & 0xFF, (bg >> 16) & 0xFF);
    let dst = core::slice::from_raw_parts_mut(bits as *mut u8, px * 4);
    // "Opaque pixels copy through" was true of the arithmetic and false of the cost: the loop
    // below still ran three multiplies and three divides per pixel to arrive at its own input.
    // A photo is always fully opaque, so ask once (or trust a caller who already knows), then
    // take the plain swizzle when there is no transparency to honour.
    if opaque.unwrap_or_else(|| (0..px).all(|i| rgba[i * 4 + 3] == 255)) {
        crate::dib::swap_rb_opaque(rgba, dst);
        return Some(hbmp);
    }
    for i in 0..px {
        let r = rgba[i * 4] as u32;
        let g = rgba[i * 4 + 1] as u32;
        let b = rgba[i * 4 + 2] as u32;
        let a = rgba[i * 4 + 3] as u32;
        // out = (src*a + bg*(255-a)) / 255, rounded.
        let comp = |s: u32, d: u32| (((s * a) + (d * (255 - a)) + 127) / 255) as u8;
        dst[i * 4] = comp(b, bg_b); // B
        dst[i * 4 + 1] = comp(g, bg_g); // G
        dst[i * 4 + 2] = comp(r, bg_r); // R
        dst[i * 4 + 3] = 255;
    }
    Some(hbmp)
}

#[cfg(test)]
mod dib_tests;
#[cfg(test)]
mod responsiveness_contract_tests;
#[cfg(test)]
mod worker_tests;
