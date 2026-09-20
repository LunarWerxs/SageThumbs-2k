//! Creating the About box's controls and the bitmaps they show.

use super::*;

/// The LunarWerx Studios wordmark bytes + aspect ratio for the active theme: the LIGHT
/// (white) variant on the dark card, the DARK (navy) variant on the light card. Picking the
/// matching variant means each is legible composited straight onto its theme background — no
/// dark backing chip.
pub(super) fn lw_logo() -> (&'static [u8], f32) {
    if is_dark() {
        (LW_LOGO_PNG, 1680.0 / 273.0)
    } else {
        (LW_LOGO_DARK_PNG, 4911.0 / 941.0)
    }
}

/// The themed LunarWerx wordmark sized to `w`×`h`. A STATIC's SS_BITMAP ignores alpha (it
/// BitBlts), so the transparent art is composited onto the card background — seamless in both
/// themes (dark bg in dark mode, light bg in light mode).
pub(super) unsafe fn lw_logo_hbitmap(w: u32, h: u32) -> Option<HBITMAP> {
    let (bytes, _) = lw_logo();
    let logo = image::load_from_memory(bytes)
        .ok()?
        .resize_exact(w, h, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    let base = DARK_BG();
    let mut out = image::RgbaImage::from_pixel(
        w,
        h,
        image::Rgba([color_r(base), color_g(base), color_b(base), 255]),
    );
    image::imageops::overlay(&mut out, &logo, 0, 0);
    sagethumbs2k_core::app_image::rgba_to_hbitmap(w, h, out.as_raw())
        .map(|h| HBITMAP(h as *mut c_void))
}

/// The GitHub mark at `px`², tinted `fg` and composited over `fill` (the pill face),
/// so it can be BitBlt'd straight onto the pill with no alpha-blend. The source PNG
/// is a white silhouette whose alpha carries the shape.
pub(super) unsafe fn github_icon_hbitmap(px: u32, fill: COLORREF, fg: COLORREF) -> Option<HBITMAP> {
    let src = image::load_from_memory(GH_PNG)
        .ok()?
        .resize_exact(px.max(1), px.max(1), image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    let (fr, fgc, fb) = (color_r(fill), color_g(fill), color_b(fill));
    let (gr, gg, gb) = (color_r(fg), color_g(fg), color_b(fg));
    let mut out = image::RgbaImage::new(src.width(), src.height());
    for (o, p) in out.pixels_mut().zip(src.pixels()) {
        let a = p[3] as u32; // octocat coverage
        let mix = |dst: u8, on: u8| ((on as u32 * a + dst as u32 * (255 - a)) / 255) as u8;
        *o = image::Rgba([mix(fr, gr), mix(fgc, gg), mix(fb, gb), 255]);
    }
    sagethumbs2k_core::app_image::rgba_to_hbitmap(out.width(), out.height(), out.as_raw())
        .map(|h| HBITMAP(h as *mut c_void))
}

pub(super) unsafe fn build_about(hwnd: HWND, hinst: HINSTANCE) {
    // The GitHub mark, built first (composited on the resting pill face) so the very
    // first pill paint already has it.
    let icon_px = s(hwnd, ICON).max(1) as u32;
    let icon = github_icon_hbitmap(icon_px, BTN_FACE(), DARK_TEXT());
    let st = about_state(hwnd);
    if !st.is_null() {
        (*st).gh_icon = icon;
    }

    // Eye logo, centered near the top.
    let logo = ctl(
        hwnd,
        STATIC,
        "",
        WINDOW_STYLE(SS_BITMAP),
        (CW - 72) / 2,
        20,
        72,
        72,
        -1,
        hinst,
    );
    if let Some(hbmp) = load_art(LOGO_PNG, "logo.png", 72, 72) {
        set_static_bitmap(logo, hbmp);
        if !st.is_null() {
            (*st).logo_icon = Some(hbmp); // the STATIC won't free it — we do, in WM_NCDESTROY
        }
    }

    // Product title — big + bold — then the muted subtitle.
    let title = ctl(
        hwnd,
        STATIC,
        "SageThumbs 2K",
        WINDOW_STYLE(SS_CENTER),
        20,
        100,
        CW - 40,
        34,
        -1,
        hinst,
    );
    SendMessageW(
        title,
        WM_SETFONT,
        Some(WPARAM(gui_font_sized(hwnd, 26, 700).0 as usize)),
        Some(LPARAM(1)),
    );
    ctl(
        hwnd,
        STATIC,
        t("about_subtitle"),
        WINDOW_STYLE(SS_CENTER),
        20,
        138,
        CW - 40,
        18,
        ID_SUBTITLE,
        hinst,
    );

    // The two status pills, centered as a group. Each pill's width is fixed (the
    // version is constant; the status pill is sized to its widest possible text), so
    // the owner-draw just centers content inside.
    let ver = format!("v{}", env!("CARGO_PKG_VERSION"));
    let ver_w = 14 + ICON + 7 + text_width(hwnd, &ver) + 14;
    let cand = [
        t("about_checking").to_string(),
        t("about_uptodate").to_string(),
        t("about_check_failed").to_string(),
        format!("{} 99.99.99", t("about_update")),
    ];
    let max_tw = cand.iter().map(|c| text_width(hwnd, c)).max().unwrap_or(80);
    let status_w = 14 + 10 + 8 + max_tw + 14;
    let gap = 12;
    let gx = (CW - (ver_w + gap + status_w)) / 2;
    let pill = WINDOW_STYLE(SS_OWNERDRAW | SS_NOTIFY);
    ctl(
        hwnd,
        STATIC,
        "",
        pill,
        gx,
        174,
        ver_w,
        30,
        ID_VER_PILL,
        hinst,
    );
    ctl(
        hwnd,
        STATIC,
        "",
        pill,
        gx + ver_w + gap,
        174,
        status_w,
        30,
        ID_STATUS_PILL,
        hinst,
    );

    // "Send feedback", centered on its own row under the status pills. Sized to its
    // label (same 14px end padding as the others) so the stadium never looks stretched.
    let fb = t("fb_pill");
    let fb_w = 14 + text_width(hwnd, fb) + 14;
    ctl(
        hwnd,
        STATIC,
        "",
        pill,
        (CW - fb_w) / 2,
        212,
        fb_w,
        30,
        ID_FEEDBACK_PILL,
        hinst,
    );

    // Bottom-left: license + licence-state + copyright (muted via WM_CTLCOLORSTATIC).
    ctl(
        hwnd,
        STATIC,
        "PolyForm Noncommercial 1.0.0",
        WINDOW_STYLE(0),
        22,
        250,
        210,
        16,
        ID_LICENSE,
        hinst,
    );
    ctl(
        hwnd,
        STATIC,
        &crate::settings_dlg::licence_state_line(&crate::license::snapshot()),
        WINDOW_STYLE(0),
        22,
        268,
        228,
        16,
        ID_LICENCE_STATE,
        hinst,
    );
    ctl(
        hwnd,
        STATIC,
        "\u{00a9} 2026 Lunarwerx",
        WINDOW_STYLE(0),
        22,
        286,
        210,
        16,
        ID_COPYRIGHT,
        hinst,
    );

    // Bottom-right: the clickable LunarWerx Studios wordmark. The two theme variants have
    // different aspect ratios, so size the control to the active one (fixed height, width
    // from the aspect) — no squish — and right-anchor it. y nudged down 10 (252→262) to
    // stay roughly centered against the bottom-left block's now-three lines.
    let (_, lw_aspect) = lw_logo();
    let lw_h = 26;
    let lw_w = (lw_h as f32 * lw_aspect).round() as i32;
    let lw = ctl(
        hwnd,
        STATIC,
        "",
        WINDOW_STYLE(SS_BITMAP | SS_NOTIFY),
        CW - 22 - lw_w,
        262,
        lw_w,
        lw_h,
        ID_LW_LOGO,
        hinst,
    );
    if let Some(hbmp) = lw_logo_hbitmap(lw_w as u32, lw_h as u32) {
        set_static_bitmap(lw, hbmp);
        if !st.is_null() {
            (*st).lw_icon = Some(hbmp); // ditto — freed in WM_NCDESTROY
        }
    }
}
