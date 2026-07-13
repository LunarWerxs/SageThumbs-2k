//! DPI-aware scaling + GUI fonts (extracted from win.rs; behavior unchanged).

use core::ffi::c_void;
use std::sync::OnceLock;

use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, GetStockObject, DEFAULT_GUI_FONT, HFONT,
};
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
    if ov > 0 { ov } else { unsafe { GetDpiForWindow(hwnd) as i32 } }
}

/// Scale a 96-DPI design pixel value `v` to the window's current DPI.
pub(crate) fn dpi_scale(hwnd: HWND, v: i32) -> i32 {
    dpi_scale_dpi(v, effective_dpi(hwnd))
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

/// A slightly smaller, semibold variant of the GUI font for the owner-drawn
/// section headers — gives them a typographic step-down from the body labels.
/// Cached per DPI; falls back to [`gui_font_for`] if the metrics query fails.
pub(crate) unsafe fn gui_font_header(hwnd: HWND) -> HFONT {
    let dpi = GetDpiForWindow(hwnd);
    let dpi = if dpi == 0 { 96 } else { dpi };
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
        let mut lf = ncm.lfMessageFont;
        // lfWidth = 0 lets GDI choose the natural width for the height — otherwise a
        // non-zero width carried over while we shrink the height distorts the aspect
        // ("squished") and a synthesized semibold compounds it. Keep the message
        // font's own weight, just a touch smaller.
        lf.lfWidth = 0;
        lf.lfHeight = MulDiv(lf.lfHeight, 19, 20); // ~5% smaller than body
        CreateFontIndirectW(&lf)
    } else {
        gui_font_for(hwnd)
    };
    guard.push((dpi, hf.0 as usize));
    hf
}

/// A larger semibold font for the v3 category page-header title (~22px @ 96dpi).
/// Cached per DPI; falls back to [`gui_font_for`] if the metrics query fails.
pub(crate) unsafe fn gui_font_title(hwnd: HWND) -> HFONT {
    let dpi = GetDpiForWindow(hwnd);
    let dpi = if dpi == 0 { 96 } else { dpi };
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
        let mut lf = ncm.lfMessageFont;
        lf.lfWidth = 0;
        lf.lfHeight = -MulDiv(22, dpi as i32, 96);
        lf.lfWeight = 600; // FW_SEMIBOLD
        CreateFontIndirectW(&lf)
    } else {
        gui_font_for(hwnd)
    };
    guard.push((dpi, hf.0 as usize));
    hf
}

/// A GUI font at an arbitrary point/pixel size and weight, DPI-scaled and cached
/// per `(px, weight, dpi)`. `px` is the cap height in 96-DPI design pixels (scaled
/// to `hwnd`'s DPI); `weight` is an `lfWeight` (e.g. 700 = FW_BOLD). Used by the
/// About box for its big bold product title. Falls back to [`gui_font_for`] if the
/// metrics query fails. Caching keeps repeated dialog opens from leaking HFONTs.
pub(crate) unsafe fn gui_font_sized(hwnd: HWND, px: i32, weight: i32) -> HFONT {
    let dpi = GetDpiForWindow(hwnd);
    let dpi = if dpi == 0 { 96 } else { dpi };
    // (px, weight, dpi, HFONT-as-usize) memo rows.
    #[allow(clippy::type_complexity)]
    static FONTS: OnceLock<std::sync::Mutex<Vec<(i32, i32, u32, usize)>>> = OnceLock::new();
    let cache = FONTS.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut guard = cache.lock().unwrap();
    if let Some(&(_, _, _, p)) = guard.iter().find(|(x, w, d, _)| *x == px && *w == weight && *d == dpi) {
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
        lf.lfWidth = 0; // let GDI pick the natural width for the height (no squish)
        lf.lfHeight = -MulDiv(px, dpi as i32, 96);
        lf.lfWeight = weight;
        CreateFontIndirectW(&lf)
    } else {
        gui_font_for(hwnd)
    };
    guard.push((px, weight, dpi, hf.0 as usize));
    hf
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
