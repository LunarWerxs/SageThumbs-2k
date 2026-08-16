//! DPI-aware scaling + GUI fonts (extracted from win.rs; behavior unchanged).

use core::ffi::c_void;
use std::sync::OnceLock;

use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{CreateFontIndirectW, GetStockObject, DEFAULT_GUI_FONT, HFONT};
use windows::Win32::System::WindowsProgramming::MulDiv;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::WindowsAndMessaging::*;

pub(crate) unsafe fn gui_font() -> HFONT {
    static FONT: OnceLock<usize> = OnceLock::new();
    let p = *FONT.get_or_init(|| {
        let mut ncm = NONCLIENTMETRICSW {
            cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        let hf = if SystemParametersInfoW(
            SPI_GETNONCLIENTMETRICS,
            ncm.cbSize,
            Some(&mut ncm as *mut _ as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
        {
            CreateFontIndirectW(&ncm.lfMessageFont)
        } else {
            HFONT(GetStockObject(DEFAULT_GUI_FONT).0)
        };
        hf.0 as usize
    });
    HFONT(p as *mut c_void)
}

// ---- DPI scaling --------------------------------------------------------
// The app declares PerMonitorV2 but lays out in 96-DPI pixels. Every layout
// coordinate/size is routed through `dpi_scale`, so a non-96 monitor gets a
// proportionally larger layout. SAFETY PROPERTY: at 96 DPI the factor is 1.0
// (`MulDiv(v, 96, 96) == v`), so a standard display is byte-identical to before.

/// Scale a 96-DPI design pixel value `v` to an explicit `dpi`. `MulDiv(v, dpi,
/// 96)` — exactly the identity when dpi == 96, which is the safety property that
/// keeps a standard display byte-identical.
pub(crate) fn dpi_scale_dpi(v: i32, dpi: i32) -> i32 {
    let dpi = if dpi == 0 { 96 } else { dpi };
    unsafe { MulDiv(v, dpi, 96) }
}

/// Headless-shot DPI override. 0 (the production default) means "use the real
/// per-window DPI"; a positive value forces [`dpi_scale`] / [`gui_font_for`] to
/// that DPI so `--shot --window preview --dpi N` can capture a high-DPI layout
/// off-screen without a physical high-DPI monitor. Only ever set from the shot
/// code path, so the 96-DPI identity (and every production display) is unchanged.
static DPI_OVERRIDE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Force the DPI used by [`dpi_scale`] / [`gui_font_for`] (headless shot capture only).
pub(crate) fn set_dpi_override(dpi: i32) {
    DPI_OVERRIDE.store(dpi.max(0), std::sync::atomic::Ordering::Relaxed);
}

/// The effective DPI for `hwnd`: the shot override when one is set, else the real
/// per-window DPI (0 on a bad HWND → callers treat as 96).
fn effective_dpi(hwnd: HWND) -> i32 {
    let ov = DPI_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed);
    if ov > 0 {
        ov
    } else {
        unsafe { GetDpiForWindow(hwnd) as i32 }
    }
}

/// Scale a 96-DPI design pixel value `v` to the window's current DPI.
pub(crate) fn dpi_scale(hwnd: HWND, v: i32) -> i32 {
    dpi_scale_dpi(v, effective_dpi(hwnd))
}

/// The inverse of [`dpi_scale`]: a device pixel value on `hwnd`'s monitor back to 96-DPI design
/// px. Used when PERSISTING a size the user dragged out (the Quick preview window), so the stored
/// number is scaling-independent and reopens at the same apparent size on another display. Keeps
/// the same 96-DPI identity property (`MulDiv(v, 96, 96) == v`).
pub(crate) fn dpi_unscale(hwnd: HWND, v: i32) -> i32 {
    let dpi = effective_dpi(hwnd);
    let dpi = if dpi == 0 { 96 } else { dpi };
    unsafe { MulDiv(v, 96, dpi) }
}

/// Create a DPI-aware GUI font for `hwnd`: the system message font with its
/// height scaled to the window's DPI (via SystemParametersInfoForDpi, which
/// returns the metrics already sized for that DPI). Cached per DPI. Falls back
/// to the plain 96-DPI [`gui_font`] if the query fails. At 96 DPI this matches
/// `gui_font` (identity), keeping a standard display unchanged.
pub(crate) unsafe fn gui_font_for(hwnd: HWND) -> HFONT {
    let dpi = effective_dpi(hwnd) as u32; // honours the headless-shot DPI override
    let dpi = if dpi == 0 { 96 } else { dpi };
    if dpi == 96 {
        return gui_font();
    }
    // Cache one scaled font per DPI value (handful of distinct DPIs in practice).
    static FONTS: OnceLock<std::sync::Mutex<Vec<(u32, usize)>>> = OnceLock::new();
    let cache = FONTS.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut guard = cache.lock().unwrap();
    if let Some(&(_, p)) = guard.iter().find(|(d, _)| *d == dpi) {
        return HFONT(p as *mut c_void);
    }
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    let hf = if SystemParametersInfoForDpi(
        SPI_GETNONCLIENTMETRICS.0,
        ncm.cbSize,
        Some(&mut ncm as *mut _ as *mut c_void),
        0,
        dpi,
    )
    .is_ok()
    {
        CreateFontIndirectW(&ncm.lfMessageFont)
    } else {
        gui_font() // fall back to the unscaled font
    };
    guard.push((dpi, hf.0 as usize));
    hf
}

/// Which pre-baked GUI-font "look" a [`gui_font_variant`] cache slot/LOGFONT tweak
/// is for. `Sized` carries its own `(px, weight)` since, unlike the other two, it has
/// no single fixed shape.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FontVariant {
    /// Slightly smaller, same weight as the body font (owner-drawn section headers).
    Header,
    /// Larger, semibold (~22px @ 96dpi; the v3 category page-header title).
    Title,
    /// Arbitrary (cap-height px, `lfWeight`) — the About box's big bold title.
    Sized(i32, i32),
}

/// One shared DPI-scaled, cached GUI-font builder behind [`gui_font_header`],
/// [`gui_font_title`], and [`gui_font_sized`]. Each of the three used to carry its
/// own `OnceLock` cache + `SystemParametersInfoForDpi`/`NONCLIENTMETRICSW` dance +
/// fallback, differing only in the LOGFONT tweak below (now the `match`) — collapsing
/// them to one site is also what fixes the headless-shot DPI-override bug: all three
/// used to read `GetDpiForWindow` directly instead of going through [`effective_dpi`]
/// like [`gui_font_for`] already did, so `--shot --dpi N` mixed real-DPI
/// headers/titles/big text with forced-DPI body text. There is now exactly one place
/// that reads the DPI for any of them, so that class of bug can't recur per-variant.
unsafe fn gui_font_variant(hwnd: HWND, variant: FontVariant) -> HFONT {
    let dpi = effective_dpi(hwnd) as u32; // honours the headless-shot DPI override
    let dpi = if dpi == 0 { 96 } else { dpi };

    // (variant, dpi, HFONT-as-usize) memo rows — one shared cache for all three looks.
    static FONTS: OnceLock<std::sync::Mutex<Vec<(FontVariant, u32, usize)>>> = OnceLock::new();
    let cache = FONTS.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut guard = cache.lock().unwrap();
    if let Some(&(_, _, p)) = guard.iter().find(|(v, d, _)| *v == variant && *d == dpi) {
        return HFONT(p as *mut c_void);
    }

    let mut ncm = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    let hf = if SystemParametersInfoForDpi(
        SPI_GETNONCLIENTMETRICS.0,
        ncm.cbSize,
        Some(&mut ncm as *mut _ as *mut c_void),
        0,
        dpi,
    )
    .is_ok()
    {
        let mut lf = ncm.lfMessageFont;
        match variant {
            FontVariant::Header => {
                // lfWidth = 0 lets GDI choose the natural width for the height —
                // otherwise a non-zero width carried over while we shrink the height
                // distorts the aspect ("squished") and a synthesized semibold
                // compounds it. Keep the message font's own weight, just a touch
                // smaller.
                lf.lfWidth = 0;
                lf.lfHeight = MulDiv(lf.lfHeight, 19, 20); // ~5% smaller than body
            }
            FontVariant::Title => {
                lf.lfWidth = 0;
                lf.lfHeight = -MulDiv(22, dpi as i32, 96);
                lf.lfWeight = 600; // FW_SEMIBOLD
            }
            FontVariant::Sized(px, weight) => {
                lf.lfWidth = 0; // let GDI pick the natural width for the height (no squish)
                lf.lfHeight = -MulDiv(px, dpi as i32, 96);
                lf.lfWeight = weight;
            }
        }
        CreateFontIndirectW(&lf)
    } else {
        gui_font_for(hwnd) // fall back to the unscaled/plain DPI-scaled font
    };
    guard.push((variant, dpi, hf.0 as usize));
    hf
}

/// A slightly smaller, semibold variant of the GUI font for the owner-drawn
/// section headers — gives them a typographic step-down from the body labels.
/// Cached per DPI; falls back to [`gui_font_for`] if the metrics query fails.
pub(crate) unsafe fn gui_font_header(hwnd: HWND) -> HFONT {
    gui_font_variant(hwnd, FontVariant::Header)
}

/// A larger semibold font for the v3 category page-header title (~22px @ 96dpi).
/// Cached per DPI; falls back to [`gui_font_for`] if the metrics query fails.
pub(crate) unsafe fn gui_font_title(hwnd: HWND) -> HFONT {
    gui_font_variant(hwnd, FontVariant::Title)
}

/// A GUI font at an arbitrary point/pixel size and weight, DPI-scaled and cached
/// per `(px, weight, dpi)`. `px` is the cap height in 96-DPI design pixels (scaled
/// to `hwnd`'s DPI); `weight` is an `lfWeight` (e.g. 700 = FW_BOLD). Used by the
/// About box for its big bold product title. Falls back to [`gui_font_for`] if the
/// metrics query fails. Caching keeps repeated dialog opens from leaking HFONTs.
pub(crate) unsafe fn gui_font_sized(hwnd: HWND, px: i32, weight: i32) -> HFONT {
    gui_font_variant(hwnd, FontVariant::Sized(px, weight))
}

/// Minimal WM_DPICHANGED handler shared by every top-level wndproc: move/resize
/// the window to the suggested rect Windows hands us in `lparam`. The controls
/// are laid out once at WM_CREATE for the creation DPI; this keeps the frame
/// correct when the window is dragged across monitors with different DPIs.
pub(crate) unsafe fn wm_dpichanged(hwnd: HWND, lparam: LPARAM) {
    if lparam.0 == 0 {
        return;
    }
    let r = &*(lparam.0 as *const RECT);
    let _ = SetWindowPos(
        hwnd,
        None,
        r.left,
        r.top,
        r.right - r.left,
        r.bottom - r.top,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
}

/// Serialises the tests that drive the process-wide DPI override.
///
/// `set_dpi_override` writes ONE global that [`effective_dpi`] reads, and cargo runs a
/// binary's tests on many threads at once, so two tests setting different overrides race:
/// the second setter silently changes the first one's answer, or the first one's guard
/// resets the override to 0 while the second is still asserting. Not hypothetical, this is
/// exactly what made `gui_font_title_and_sized_honor_the_shot_dpi_override_not_a_bogus_
/// window_dpi` pass on its own and fail in the full run.
///
/// It lives here, beside `set_dpi_override`, rather than in each test module: a second
/// module previously copied the reset-on-drop guard because this one was "unreachable from
/// there", and duplicating it is what let the two tests race in the first place. Any new
/// test that touches the override must take this lock BEFORE setting it.
#[cfg(test)]
pub(crate) static DPI_OVERRIDE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Holds [`DPI_OVERRIDE_TEST_LOCK`] for a test's lifetime and restores the override to 0 on
/// drop, panic included. Acquire it BEFORE calling `set_dpi_override`.
#[cfg(test)]
pub(crate) struct DpiOverrideGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

#[cfg(test)]
impl DpiOverrideGuard {
    pub(crate) fn acquire() -> Self {
        // A test that panicked while holding this poisoned the mutex; the override is still
        // reset by the guard's Drop, so the lock is safe to keep using.
        Self(
            DPI_OVERRIDE_TEST_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }
}

#[cfg(test)]
impl Drop for DpiOverrideGuard {
    fn drop(&mut self) {
        set_dpi_override(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Gdi::{GetObjectW, HGDIOBJ, LOGFONTW};

    fn logfont_of(hf: HFONT) -> LOGFONTW {
        let mut lf = LOGFONTW::default();
        unsafe {
            GetObjectW(
                HGDIOBJ(hf.0),
                std::mem::size_of::<LOGFONTW>() as i32,
                Some(&mut lf as *mut _ as *mut c_void),
            );
        }
        lf
    }

    /// `gui_font_title`/`gui_font_sized` (and `gui_font_header`, exercised the same
    /// way elsewhere) used to read `GetDpiForWindow(hwnd)` directly instead of going
    /// through `effective_dpi`, so a headless `--shot --dpi N` capture forced the body
    /// font but left these three at the window's REAL DPI. A garbage HWND makes the
    /// bug observable: `GetDpiForWindow` on it returns 0, which every one of these
    /// functions' own `if dpi == 0 { 96 }` fallback turns into the exact same 96 a
    /// caller with NO override at all would get — so before the fix, setting the
    /// override had no effect on their output at all with this HWND.
    #[test]
    fn gui_font_title_and_sized_honor_the_shot_dpi_override_not_a_bogus_window_dpi() {
        let bogus_hwnd = HWND(usize::MAX as *mut c_void);
        let _guard = DpiOverrideGuard::acquire(); // BEFORE the set: see the lock's doc
        set_dpi_override(240); // 2.5x

        let title = logfont_of(unsafe { gui_font_title(bogus_hwnd) });
        let sized = logfont_of(unsafe { gui_font_sized(bogus_hwnd, 20, 700) });

        // Both heights are fully determined by `dpi` alone (`-MulDiv(px, dpi, 96)`),
        // so this is exact, not approximate: -55 (title) / -50 (sized) at dpi=240 vs
        // -22 / -20 at the 96-DPI fallback the pre-fix code would have produced.
        assert_eq!(
            title.lfHeight, -55,
            "gui_font_title ignored the DPI override"
        );
        assert_eq!(
            sized.lfHeight, -50,
            "gui_font_sized ignored the DPI override"
        );
        assert_eq!(sized.lfWeight, 700);
    }
}
