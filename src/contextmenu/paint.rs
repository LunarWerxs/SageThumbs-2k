//! Menu-preview rendering: caption font/metrics, theme colours, the checker backdrop and
//! the owner-draw paint itself.
//!
//! Split out of `contextmenu.rs` 2026-07-31 (pure move).

use super::*;

/// The actual menu font (`SPI_GETNONCLIENTMETRICS.lfMenuFont`, e.g. Segoe UI on
/// Win11), so the caption matches the surrounding menu items exactly — the stock
/// `DEFAULT_GUI_FONT` is an old, mismatched typeface. `Some` must be deleted by
/// the caller; `None` means "fall back to the stock GUI font" (do NOT delete).
pub(crate) unsafe fn menu_font() -> Option<HFONT> {
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: core::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    let ok = SystemParametersInfoW(
        SPI_GETNONCLIENTMETRICS,
        ncm.cbSize,
        Some(&mut ncm as *mut _ as *mut core::ffi::c_void),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
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

/// The menu font, created once per process and never freed — preview-tile
/// composition may occur while a live menu still references the bitmap. Same
/// never-free rationale as
/// [`menu_logo`]; the classic-menu host is short-lived. Falls back to
/// [`menu_font`] on the cache-miss path and to the stock GUI font if even that
/// fails. Returns an HFONT the caller must NOT delete.
pub(crate) fn menu_font_cached() -> HFONT {
    use std::sync::OnceLock;
    static FONT: OnceLock<isize> = OnceLock::new();
    let h = *FONT.get_or_init(|| {
        unsafe { menu_font() }
            .map(|f| f.0 as isize)
            .unwrap_or_else(|| unsafe { GetStockObject(DEFAULT_GUI_FONT) }.0 as isize)
    });
    HFONT(h as *mut core::ffi::c_void)
}

/// Select the cached menu font into `hdc`; returns the prior font to restore.
/// The font is process-cached (never freed), so there is nothing to delete —
/// unlike the old per-call font, callers must NOT delete the returned font.
pub(crate) unsafe fn select_menu_font(hdc: windows::Win32::Graphics::Gdi::HDC) -> HGDIOBJ {
    SelectObject(hdc, HGDIOBJ(menu_font_cached().0))
}

/// Widest caption line in px (measured with the real menu font), capped.
pub(crate) unsafe fn caption_width_of(p: &Preview) -> i32 {
    let hdc = CreateCompatibleDC(None);
    let old = select_menu_font(hdc);
    let mut max_w = 0i32;
    for line in [&p.name, &p.info] {
        let mut sz = SIZE::default();
        if !line.is_empty() && GetTextExtentPoint32W(hdc, line, &mut sz).as_bool() {
            max_w = max_w.max(sz.cx);
        }
    }
    SelectObject(hdc, old);
    let _ = DeleteDC(hdc);
    max_w.min(CAPTION_MAX) // cap so an absurdly long name can't blow the menu up
}

/// Diagnostics: render the preview tile to a PNG via the SAME compositing path
/// the menu uses (`paint_preview`), so it can be eyeballed without driving a real
/// menu. `bg` overrides the background (so light/dark menus can both be
/// previewed); pass `None` to use the live menu theme colors.
#[doc(hidden)]
pub fn render_preview_png(path: &str, out_png: &str, bg: Option<u32>) -> bool {
    unsafe {
        let Some(p) = build_preview(path, None) else {
            return false;
        };
        let text_w = caption_width_of(&p);
        let iw = p.w.max(text_w).max(72) + 12;
        let ih = p.h + 48;

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

        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = iw;
        bmi.bmiHeader.biHeight = -ih; // top-down
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
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
        for i in 0..(iw * ih) as usize {
            rgba[i * 4] = src[i * 4 + 2]; // R
            rgba[i * 4 + 1] = src[i * 4 + 1]; // G
            rgba[i * 4 + 2] = src[i * 4]; // B
            rgba[i * 4 + 3] = 255;
        }
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
/// Reads `AppsUseLightTheme`, matching `bin/app/dark.rs` and `previewhandler.rs`.
/// It used to read `SystemUsesLightTheme` — that key is the TASKBAR/Start theme,
/// which is independent: "dark apps + light taskbar" is a common setup, and there
/// it reported light while Explorer's menu was dark, so the preview tile was baked
/// white-on-dark (reported from a pt-BR Win11 machine, v1.3.1).
pub(crate) fn menu_dark() -> bool {
    windows_registry::CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_u32("AppsUseLightTheme"))
        .map(|v| v == 0)
        .unwrap_or(false)
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

/// Two subtle checkerboard shades from a base menu colour: the base nudged a few
/// levels darker and a few lighter. Their average stays ≈ `bg` (so the menu tone
/// doesn't shift) and they sit only ~16 levels apart — enough to read as
/// "transparency here" without competing with the menu. Follows light/dark/accent
/// automatically since it's derived from whatever `bg` is passed.
pub fn checker_shades(bg: u32) -> (u32, u32) {
    let ch = |shift: u32| (bg >> shift) & 0xFF; // COLORREF is 0x00BBGGRR
    let (r, g, b) = (ch(0), ch(8), ch(16));
    let darker = |c: u32| c.saturating_sub(8);
    let lighter = |c: u32| (c + 8).min(255);
    let pack = |r: u32, g: u32, b: u32| r | (g << 8) | (b << 16);
    (
        pack(darker(r), darker(g), darker(b)),
        pack(lighter(r), lighter(g), lighter(b)),
    )
}

/// Fill `rc` with a two-tone checkerboard of `cell`-px squares — the backdrop a thumbnail is
/// alpha-blended onto, so transparent pixels reveal the pattern instead of disappearing into the
/// flat background colour.
///
/// `cell` is a caller choice because the two surfaces that use this are different sizes: the menu
/// tile is a ~72px thumbnail where 8px reads as texture, while the Quick preview window is
/// full-size and wants a DPI-scaled, visibly larger square.
///
/// # Safety
///
/// `hdc` must be a valid device context that stays alive for the call. Nothing is retained.
pub unsafe fn fill_checker(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    rc: &RECT,
    c0: u32,
    c1: u32,
    cell: i32,
) {
    let (left, top) = (rc.left, rc.top);
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    let cell = cell.max(2);
    let b0 = CreateSolidBrush(COLORREF(c0));
    let b1 = CreateSolidBrush(COLORREF(c1));
    FillRect(
        hdc,
        &RECT {
            left,
            top,
            right: left + w,
            bottom: top + h,
        },
        b0,
    );
    let mut y = 0;
    while y < h {
        let mut x = 0;
        while x < w {
            if ((x / cell) + (y / cell)) & 1 == 1 {
                let r = RECT {
                    left: left + x,
                    top: top + y,
                    right: left + (x + cell).min(w),
                    bottom: top + (y + cell).min(h),
                };
                FillRect(hdc, &r, b1);
            }
            x += cell;
        }
        y += cell;
    }
    let _ = DeleteObject(b0.into());
    let _ = DeleteObject(b1.into());
}

/// Paint the preview into `rc` of `hdc`: thumbnail centered on top, name + info
/// lines under, with explicit `bg`/`fg` colors. Used both by the off-screen
/// compositor ([`preview_hbitmap`]) and the diagnostic PNG renderer.
pub(crate) unsafe fn paint_preview(hdc: HDC, rc: RECT, p: &Preview, bg: u32, fg: u32) {
    let brush = CreateSolidBrush(COLORREF(bg));
    FillRect(hdc, &rc, brush);
    let _ = DeleteObject(brush.into());

    // Thumbnail, horizontally centered. Skipped entirely for the caption-only
    // fallback tile (null bitmap / 0×0 — a file that passed the size gate but failed
    // to decode), which shows just the name + size rows below.
    let bx = rc.left + ((rc.right - rc.left) - p.w) / 2;
    let by = rc.top + 4;
    if !p.hbm.is_invalid() && p.w > 0 && p.h > 0 {
        // Subtle checkerboard behind the thumbnail so transparent images stay visible
        // against the flat menu colour (default on; toggleable in Settings).
        if settings::preview_checker() {
            let (c0, c1) = checker_shades(bg);
            let cr = RECT {
                left: bx,
                top: by,
                right: bx + p.w,
                bottom: by + p.h,
            };
            fill_checker(hdc, &cr, c0, c1, 8);
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
    let oldf = select_menu_font(hdc);
    SetTextColor(hdc, COLORREF(fg));

    let mut name = p.name.clone();
    let mut line1 = RECT {
        left: rc.left + 6,
        top: by + p.h + 2,
        right: rc.right - 6,
        bottom: by + p.h + 20,
    };
    DrawTextW(
        hdc,
        &mut name,
        &mut line1,
        DT_CENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
    );

    let mut info = p.info.clone();
    let mut line2 = RECT {
        left: rc.left + 6,
        top: line1.bottom + 1,
        right: rc.right - 6,
        bottom: line1.bottom + 19,
    };
    DrawTextW(
        hdc,
        &mut info,
        &mut line2,
        DT_CENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
    );

    SelectObject(hdc, oldf);
}

/// The preview item's pixel size: wide enough for the thumbnail and the (capped)
/// caption, tall enough for the image plus the two caption rows. Reported to the menu
/// from `WM_MEASUREITEM` and reused by the diagnostic PNG so the two can't drift.
pub(crate) unsafe fn tile_size(p: &Preview) -> (i32, i32) {
    let text_w = caption_width_of(p);
    (p.w.max(text_w).max(72) + 12, p.h + 48)
}

/// Open the file with its default app (the preview item's click action).
pub(crate) fn open_with_default(path: &str) {
    let wide = crate::wide(path);
    unsafe {
        ShellExecuteW(
            None,
            windows::core::w!("open"),
            PCWSTR(wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
}
