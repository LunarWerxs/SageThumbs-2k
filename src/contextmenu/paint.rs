//! Menu-preview rendering: caption font/metrics, theme colours, the checker backdrop and
//! the owner-draw paint itself.
//!
//! Split out of `contextmenu.rs` 2026-07-31 (pure move). DPI-aware since #61: every length
//! here is a 96-DPI design value scaled to the DPI the menu is shown at.

use super::*;
use st2k_base::checkerpx::{checker_shades, fill_checker};
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{
    GetTextMetricsW, MonitorFromPoint, MONITOR_DEFAULTTONEAREST, TEXTMETRICW,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, SystemParametersInfoForDpi, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

/// The highest DPI the tile is laid out for (Windows' own ceiling is 500%, 480 DPI). Bounds
/// the per-DPI font cache and the decoded thumbnail's size against a nonsense answer.
const MAX_MENU_DPI: u32 = 480;

/// The DPI the menu about to open is drawn at: the effective DPI of the monitor under the
/// cursor, which is where a right-click menu opens. Answered in the calling thread's own
/// DPI terms, so a DPI-unaware host (whose menus Windows stretches as a whole) gets 96 and
/// keeps the 100% tile. 96 whenever the query fails.
pub(crate) fn menu_dpi() -> u32 {
    let mut pt = POINT::default();
    let (mut x, mut y) = (0u32, 0u32);
    let ok = unsafe {
        GetCursorPos(&mut pt).is_ok()
            && GetDpiForMonitor(
                MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST),
                MDT_EFFECTIVE_DPI,
                &mut x,
                &mut y,
            )
            .is_ok()
    };
    if ok {
        clamp_dpi(x)
    } else {
        USER_DEFAULT_SCREEN_DPI
    }
}

/// Keep a DPI inside what Windows can actually show (100% to 500%).
pub(crate) fn clamp_dpi(dpi: u32) -> u32 {
    dpi.clamp(USER_DEFAULT_SCREEN_DPI, MAX_MENU_DPI)
}

/// Scale a 96-DPI design length to `dpi`, rounded to the nearest pixel (MulDiv's rule).
pub(crate) fn scale_px(v: i32, dpi: u32) -> i32 {
    let base = i64::from(USER_DEFAULT_SCREEN_DPI);
    ((i64::from(v) * i64::from(dpi) + base / 2) / base) as i32
}

/// The actual menu font (`SPI_GETNONCLIENTMETRICS.lfMenuFont`, e.g. Segoe UI on
/// Win11) at `dpi`, so the caption matches the surrounding menu items exactly — the stock
/// `DEFAULT_GUI_FONT` is an old, mismatched typeface. `SystemParametersInfoForDpi`, not the
/// plain call: inside a per-monitor-aware Explorer the plain call answers for the SYSTEM
/// DPI, the wrong size on any other monitor. `Some` must be deleted by the caller; `None`
/// means "fall back to the stock GUI font" (do NOT delete).
pub(crate) unsafe fn menu_font(dpi: u32) -> Option<HFONT> {
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: core::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    let ok = SystemParametersInfoForDpi(
        SPI_GETNONCLIENTMETRICS.0,
        ncm.cbSize,
        Some(&mut ncm as *mut _ as *mut core::ffi::c_void),
        0,
        dpi,
    )
    .is_ok();
    if ok {
        let f = CreateFontIndirectW(&ncm.lfMenuFont);
        if !f.is_invalid() {
            return Some(f);
        }
    }
    None
}

/// The menu font at `dpi`, created once per DPI per process and never freed — preview-tile
/// composition may occur while a live menu still references the bitmap. Same never-free
/// rationale as [`menu_logo`]; the classic-menu host is short-lived, and [`clamp_dpi`]
/// keeps the cache to the handful of DPIs one desk can have. Falls back to the stock GUI
/// font when [`menu_font`] fails. Returns an HFONT the caller must NOT delete.
pub(crate) fn menu_font_cached(dpi: u32) -> HFONT {
    use std::sync::{Mutex, PoisonError};
    static FONTS: Mutex<Vec<(u32, isize)>> = Mutex::new(Vec::new());
    let dpi = clamp_dpi(dpi);
    let mut fonts = FONTS.lock().unwrap_or_else(PoisonError::into_inner);
    let h = match fonts.iter().find(|(d, _)| *d == dpi) {
        Some(&(_, h)) => h,
        None => {
            let h = unsafe { menu_font(dpi) }
                .map(|f| f.0 as isize)
                .unwrap_or_else(|| unsafe { GetStockObject(DEFAULT_GUI_FONT) }.0 as isize);
            fonts.push((dpi, h));
            h
        }
    };
    HFONT(h as *mut core::ffi::c_void)
}

/// Select the cached menu font for `dpi` into `hdc`; returns the prior font to restore.
/// The font is process-cached (never freed), so there is nothing to delete —
/// unlike the old per-call font, callers must NOT delete the returned font.
pub(crate) unsafe fn select_menu_font(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    dpi: u32,
) -> HGDIOBJ {
    SelectObject(hdc, HGDIOBJ(menu_font_cached(dpi).0))
}

/// The caption as the menu font at the preview's DPI sets it: the widest line's width
/// (uncapped; [`TileMetrics::size`] applies the cap) and the font's line height.
pub(crate) unsafe fn caption_metrics(p: &Preview) -> (i32, i32) {
    let hdc = CreateCompatibleDC(None);
    let old = select_menu_font(hdc, p.dpi);
    let mut max_w = 0i32;
    for line in [&p.name, &p.info] {
        let mut sz = SIZE::default();
        if !line.is_empty() && GetTextExtentPoint32W(hdc, line, &mut sz).as_bool() {
            max_w = max_w.max(sz.cx);
        }
    }
    let mut tm = TEXTMETRICW::default();
    let line_h = if GetTextMetricsW(hdc, &mut tm).as_bool() {
        tm.tmHeight
    } else {
        0
    };
    SelectObject(hdc, old);
    let _ = DeleteDC(hdc);
    (max_w, line_h)
}

/// The tile's layout at one DPI, in device pixels: each length is its 96-DPI design value
/// scaled to that DPI, and a caption row is never shorter than the font's own line height.
/// At 96 DPI with the stock menu font this is exactly the tile every earlier version drew.
/// Before #61 these were fixed 96-DPI pixels, so at 200% a 32 px menu font was drawn into
/// 18 px rows and lost the bottom of every caption.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TileMetrics {
    /// Gap above the thumbnail.
    pub(crate) top: i32,
    /// Gap between the thumbnail and the first caption row.
    pub(crate) gap: i32,
    /// Height of one caption row.
    pub(crate) row: i32,
    /// Gap between the two caption rows.
    pub(crate) row_gap: i32,
    /// Gap below the second caption row.
    pub(crate) bottom: i32,
    /// Caption inset on each side.
    pub(crate) inset: i32,
    /// Narrowest body, so a tiny or caption-only tile still reads as a tile.
    pub(crate) min_w: i32,
    /// Widest the caption may make the tile; a longer name is cut with an ellipsis.
    pub(crate) caption_max: i32,
}

impl TileMetrics {
    pub(crate) fn at(dpi: u32, line_h: i32) -> Self {
        let s = |v| scale_px(v, dpi);
        Self {
            top: s(4),
            gap: s(2),
            row: s(18).max(line_h),
            row_gap: s(1),
            bottom: s(5),
            inset: s(6),
            min_w: s(72),
            caption_max: s(CAPTION_MAX),
        }
    }

    /// The tile for an `img_w` x `img_h` thumbnail under a caption `caption_w` wide.
    pub(crate) fn size(&self, img_w: i32, img_h: i32, caption_w: i32) -> (i32, i32) {
        let body = img_w.max(caption_w.min(self.caption_max)).max(self.min_w);
        (
            body + 2 * self.inset,
            self.top + img_h + self.gap + 2 * self.row + self.row_gap + self.bottom,
        )
    }
}

/// Diagnostics: render the preview tile to a PNG via the SAME compositing path
/// the menu uses (`paint_preview`), so it can be eyeballed without driving a real
/// menu. `bg` overrides the background (so light/dark menus can both be
/// previewed); pass `None` to use the live menu theme colors. `dpi` lays the tile out for
/// that DPI (`None`: the monitor under the cursor), so a 200% tile can be checked on a
/// 100% desk.
#[doc(hidden)]
pub fn render_preview_png(path: &str, out_png: &str, bg: Option<u32>, dpi: Option<u32>) -> bool {
    unsafe {
        let dpi = dpi.map_or_else(menu_dpi, clamp_dpi);
        let Some(p) = build_preview(path, None, None, dpi) else {
            return false;
        };
        let (iw, ih) = tile_size(&p);

        let (cbg, cfg) = match bg {
            Some(c) => {
                // Contrasting text for the chosen bg so we can preview both modes.
                let bright = ((c & 0xFF) + ((c >> 8) & 0xFF) + ((c >> 16) & 0xFF)) / 3;
                let fg = if bright > 128 {
                    0x0020_2020
                } else {
                    0x00E0_E0E0
                };
                (c, fg)
            }
            None => menu_theme_colors(),
        };

        let bmi = st2k_base::safety::top_down_bmi(iw, ih);
        let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
        let Ok(dib) = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            return false;
        };
        if bits.is_null() {
            let _ = DeleteObject(dib.into());
            return false;
        }
        let memdc = CreateCompatibleDC(None);
        let oldbmp = SelectObject(memdc, dib.into());

        paint_preview(
            memdc,
            RECT {
                left: 0,
                top: 0,
                right: iw,
                bottom: ih,
            },
            &p,
            cbg,
            cfg,
        );
        let _ = GdiFlush();

        // Compute the byte count in usize with checked math — `(iw * ih * 4) as usize` multiplies
        // as i32 first and could overflow into an undersized length for the `from_raw_parts` below
        // (unsound). In practice the dims are tiny preview sizes, but bail cleanly on anything absurd.
        let Some(n) = (iw as usize)
            .checked_mul(ih as usize)
            .and_then(|p| p.checked_mul(4))
        else {
            SelectObject(memdc, oldbmp);
            let _ = DeleteDC(memdc);
            let _ = DeleteObject(dib.into());
            return false;
        };
        let src = core::slice::from_raw_parts(bits as *const u8, n);
        let mut rgba = vec![0u8; n];
        st2k_base::dib::swap_rb_opaque(src, &mut rgba);
        SelectObject(memdc, oldbmp);
        let _ = DeleteDC(memdc);
        let _ = DeleteObject(dib.into());

        image::RgbaImage::from_raw(iw as u32, ih as u32, rgba)
            .map(|b| b.save(out_png).is_ok())
            .unwrap_or(false)
    }
}

/// Explorer's classic context menu is APP UI, so it follows the **app** theme —
/// and legacy `GetSysColor(COLOR_MENU)` does NOT update for dark mode (it always
/// returns the light gray), so a dark menu would get a glaring white preview
/// block. Detect the real menu theme from the registry instead.
///
/// Reads `AppsUseLightTheme` via the shared [`st2k_base::safety::apps_use_dark_theme`] probe,
/// matching `bin/app/dark.rs` and `previewhandler.rs`. It used to read `SystemUsesLightTheme`
/// — that key is the TASKBAR/Start theme, which is independent: "dark apps + light taskbar" is
/// a common setup, and there it reported light while Explorer's menu was dark, so the preview
/// tile was baked white-on-dark (reported from a pt-BR Win11 machine, v1.3.1).
pub(crate) fn menu_dark() -> bool {
    st2k_base::safety::apps_use_dark_theme()
}

/// The (bg, fg) baked into the preview tile so it matches the surrounding menu.
/// The Win11 dark flyout is ~#2B2B2B with near-white text; light menus use the
/// system menu colors. We bake an OPAQUE tile (we can't read the live acrylic
/// tone, so a flat match), which is the trade for not owner-drawing.
pub(crate) unsafe fn menu_theme_colors() -> (u32, u32) {
    if menu_dark() {
        (0x002B_2B2B, 0x00E0_E0E0) // Win11 dark flyout bg + light text
    } else {
        (GetSysColor(COLOR_MENU), GetSysColor(COLOR_MENUTEXT))
    }
}

/// Paint the preview into `rc` of `hdc`: thumbnail centered on top, name + info
/// lines under, with explicit `bg`/`fg` colors. Used both by the off-screen
/// compositor ([`preview_ddb`]) and the diagnostic PNG renderer.
pub(crate) unsafe fn paint_preview(hdc: HDC, rc: RECT, p: &Preview, bg: u32, fg: u32) {
    let brush = CreateSolidBrush(COLORREF(bg));
    FillRect(hdc, &rc, brush);
    let _ = DeleteObject(brush.into());

    // Thumbnail, horizontally centered. Skipped entirely for the caption-only
    // fallback tile (null bitmap / 0×0 — a file that passed the size gate but failed
    // to decode), which shows just the name + size rows below.
    let (_, line_h) = caption_metrics(p);
    let m = TileMetrics::at(p.dpi, line_h);
    let bx = rc.left + ((rc.right - rc.left) - p.w) / 2;
    let by = rc.top + m.top;
    if !p.hbm.is_invalid() && p.w > 0 && p.h > 0 {
        // Subtle checkerboard behind the thumbnail so transparent images stay visible
        // against the flat menu colour (default on; toggleable in Settings).
        // Snapshotted once when the preview was built (`Preview::checker`), not
        // re-read here — this runs on every `WM_DRAWITEM` repaint.
        if p.checker {
            let (c0, c1) = checker_shades(bg);
            let cr = RECT {
                left: bx,
                top: by,
                right: bx + p.w,
                bottom: by + p.h,
            };
            fill_checker(hdc, &cr, c0, c1, scale_px(8, p.dpi));
        }
        let mem = CreateCompatibleDC(Some(hdc));
        let old = SelectObject(mem, p.hbm.into());
        let bf = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let _ = AlphaBlend(hdc, bx, by, p.w, p.h, mem, 0, 0, p.w, p.h, bf);
        SelectObject(mem, old);
        let _ = DeleteDC(mem);
    }

    // Caption lines, in the menu's own font + text color so they match the
    // surrounding items (both legible — no dim grey).
    SetBkMode(hdc, TRANSPARENT);
    let oldf = select_menu_font(hdc, p.dpi);
    SetTextColor(hdc, COLORREF(fg));

    let mut name = p.name.clone();
    let mut line1 = RECT {
        left: rc.left + m.inset,
        top: by + p.h + m.gap,
        right: rc.right - m.inset,
        bottom: by + p.h + m.gap + m.row,
    };
    DrawTextW(
        hdc,
        &mut name,
        &mut line1,
        DT_CENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
    );

    let mut info = p.info.clone();
    let mut line2 = RECT {
        left: rc.left + m.inset,
        top: line1.bottom + m.row_gap,
        right: rc.right - m.inset,
        bottom: line1.bottom + m.row_gap + m.row,
    };
    DrawTextW(
        hdc,
        &mut info,
        &mut line2,
        DT_CENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
    );

    SelectObject(hdc, oldf);
}

/// The preview item's pixel size at its DPI: wide enough for the thumbnail and the
/// (capped) caption, tall enough for the image plus the two caption rows. Reported to the
/// menu from `WM_MEASUREITEM`, and the size of the bitmap item and the diagnostic PNG.
pub(crate) unsafe fn tile_size(p: &Preview) -> (i32, i32) {
    let (caption_w, line_h) = caption_metrics(p);
    TileMetrics::at(p.dpi, line_h).size(p.w, p.h, caption_w)
}

/// Open the file with its default app (the preview item's click action).
pub(crate) fn open_with_default(path: &str) {
    let wide = st2k_base::host::wide(path);
    unsafe {
        let ret = ShellExecuteW(
            None,
            windows::core::w!("open"),
            PCWSTR(wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
        // ShellExecuteW returns an HINSTANCE-like value > 32 on success; <= 32 is an
        // error code (see `update.rs::launch_installer_silent`). A failed launch used
        // to vanish silently — the preview tile just did nothing on click — matching
        // the pattern `launch_app` already logs for the companion-EXE launch path.
        let se_code = ret.0 as usize;
        if !shell_execute_succeeded(se_code) {
            st2k_base::safety::log(&format!(
                "open_with_default: ShellExecuteW failed for {path} (code {se_code})"
            ));
        }
    }
}

/// True for a `ShellExecuteW` return value that indicates success (`> 32`); everything
/// `<= 32` is one of its `SE_ERR_*` codes. Split out so `open_with_default`'s
/// failure-logging branch has something to unit-test without actually launching a
/// process (`ShellExecuteW` itself is not something a test should invoke).
fn shell_execute_succeeded(code: usize) -> bool {
    code > 32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_execute_return_code_threshold_is_32() {
        // 0 and 2 (ERROR_FILE_NOT_FOUND) are documented SE_ERR_* failure codes.
        assert!(!shell_execute_succeeded(0));
        assert!(!shell_execute_succeeded(2));
        assert!(!shell_execute_succeeded(32));
        // Anything above 32 is a real HINSTANCE — success.
        assert!(shell_execute_succeeded(33));
        assert!(shell_execute_succeeded(0x1000));
    }

    /// `menu_dark` must report the APP theme, read from the key Windows actually puts it in.
    ///
    /// The earlier version of this test asserted `menu_dark() == safety::apps_use_dark_theme()`,
    /// which an adversarial audit correctly called a tautology: `menu_dark`'s entire body IS
    /// that call, so the assertion compared a function to itself and could never fail. The
    /// defect worth pinning is the one this module actually shipped once, a private copy of the
    /// read that consulted the WRONG value (`SystemUsesLightTheme`, which is the taskbar and
    /// start menu, not apps). So the expectation is derived INDEPENDENTLY here, from the key by
    /// name, and a re-added copy reading the wrong one now fails.
    #[test]
    fn menu_dark_reads_the_apps_theme_value_not_the_system_one() {
        let expected = windows_registry::CURRENT_USER
            .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
            .and_then(|k| k.get_u32("AppsUseLightTheme"))
            .map(|v| v == 0)
            .unwrap_or(false);
        assert_eq!(
            menu_dark(),
            expected,
            "menu_dark disagrees with AppsUseLightTheme, so it is reading something else"
        );
    }
}
