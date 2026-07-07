//! The About box — the "2026" card.
//!
//! A compact, owner-drawn popup that mirrors the product mock: the eye logo, the
//! product title + subtitle, two clickable status *pills* (a GitHub version chip
//! that opens the repo, and a live "Up to date" update-check chip), the license /
//! copyright in the bottom-left, and the clickable LunarWerx Studios wordmark in
//! the bottom-right. The update check runs on a worker thread when the box opens
//! and again whenever the user clicks the status pill, so the chip is never stale.

use core::ffi::c_void;

use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    Arc, BitBlt, CreateCompatibleDC, CreatePen, DeleteDC, DeleteObject, DrawTextW, Ellipse, FillRect,
    GetStockObject, GetTextExtentPoint32W, InvalidateRect, RoundRect, SelectObject, SetBkColor,
    SetBkMode, SetDCBrushColor, SetDCPenColor, SetTextColor, DC_BRUSH, DC_PEN, DT_LEFT,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HBITMAP, HBRUSH, HDC, HGDIOBJ, PS_SOLID, SRCCOPY,
    TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::DRAWITEMSTRUCT;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::{
    dark_bg_brush, dark_control, dark_ctlcolor, dark_titlebar, is_dark, rgb, BORDER_STRONG,
    BTN_FACE, DARK_BG, DARK_TEXT, DISABLED_TEXT, HEADER_TEXT,
};
use crate::update;
use crate::win::{
    app_icon, ctl, dpi_scale, dpi_scale_dpi, gui_font_for, gui_font_sized, load_art, open_url,
    set_static_bitmap, t, text_width, wide, wm_dpichanged, IDCANCEL, IDOK, SS_BITMAP, SS_CENTER,
    SS_NOTIFY, SS_OWNERDRAW, STATIC, URL_GITHUB, URL_PARENT,
};

// ---- Control IDs --------------------------------------------------------
/// The clickable LunarWerx Studios wordmark (bottom-right) → the company site.
const ID_LW_LOGO: i32 = 1119;
/// The GitHub version chip → the repo.
const ID_VER_PILL: i32 = 1201;
/// The live update-check chip → re-check (or, when an update exists, the releases page).
const ID_STATUS_PILL: i32 = 1202;
const ID_SUBTITLE: i32 = 1203;
const ID_LICENSE: i32 = 1204;
const ID_COPYRIGHT: i32 = 1205;

/// Posted from the update-check worker thread back to the About window: the check
/// finished. `WPARAM` = outcome (0 up-to-date, 1 update available, 2 failed);
/// `LPARAM` = a `Box<String>` (the newer tag) when WPARAM==1 — the handler reclaims it.
const WM_ABOUT_CHECKED: u32 = WM_APP + 1;

/// Timer id driving the status-pill spinner animation.
const SPIN_TIMER_ID: usize = 1;
/// Spinner repaint interval (ms) — ~25 fps: smooth motion, negligible cost.
const SPIN_INTERVAL_MS: u32 = 40;
/// Minimum frames the "Checking…" spinner stays up before the result is shown — a
/// deliberate ≈2 s illusion of work, since the real check is near-instant. 50 × 40 ms ≈ 2 s.
const MIN_SPIN_FRAMES: u32 = 50;

/// Client size in 96-DPI design pixels (DPI-scaled per control / for the frame).
const CW: i32 = 440;
const CH: i32 = 300;

/// Logo artwork, embedded so it always renders. A `logo.png` next to the EXE overrides.
const LOGO_PNG: &[u8] = include_bytes!("../../../assets/logo.png");
/// LunarWerx Studios wordmark — the LIGHT (white) variant on transparent (1680×273), for the
/// dark card.
const LW_LOGO_PNG: &[u8] = include_bytes!("../../../assets/lw_logo_white.png");
/// LunarWerx Studios wordmark — the DARK (navy) variant on transparent (4911×941), for the
/// light card.
const LW_LOGO_DARK_PNG: &[u8] = include_bytes!("../../../assets/lw_logo_dark.png");
/// GitHub "mark" (white silhouette on transparent) for the version pill.
const GH_PNG: &[u8] = include_bytes!("../../../assets/github_mark.png");

/// Version-pill GitHub icon size (96-dpi design px). Big enough that the octocat reads as
/// the GitHub mark and not a blob at the pill scale.
const ICON: i32 = 20;

/// The latest update-check outcome, shown by the status pill.
enum Status {
    Checking,
    UpToDate,
    Available(String),
    Failed,
}

/// Per-window state, owned via `GWLP_USERDATA`.
struct About {
    status: Status,
    /// A network check is in flight — ignore extra status-pill clicks until it lands.
    checking: bool,
    /// Spinner animation phase; doubles as the elapsed-frame counter for the faux timer.
    spin_frame: u32,
    /// A finished check whose result is held back until the faux timer's minimum elapses.
    pending: Option<Status>,
    /// The GitHub mark, pre-composited on the pill fill so the blit is seamless. Freed
    /// in `WM_NCDESTROY`.
    gh_icon: Option<HBITMAP>,
}

/// Open the About box, owned by `parent`.
pub(crate) unsafe fn show_about(parent: HWND) {
    let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
    let class = w!("SageThumbs2KAbout");
    // Idempotent: a second RegisterClassW returns 0 (already registered) — fine.
    let wc = WNDCLASSW {
        lpfnWndProc: Some(about_wndproc),
        hInstance: hinst,
        lpszClassName: class,
        hIcon: app_icon().unwrap_or_default(),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: dark_bg_brush(), // theme-aware: light bg in light, dark bg in dark
        ..Default::default()
    };
    RegisterClassW(&wc);

    // Size the frame so the *client* area is exactly the design size, scaled to the
    // parent's DPI (identity at 96 → standard displays are unchanged).
    let dpi = GetDpiForWindow(parent) as i32;
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
    let exstyle = WS_EX_DLGMODALFRAME;
    let mut rc = RECT { left: 0, top: 0, right: dpi_scale_dpi(CW, dpi), bottom: dpi_scale_dpi(CH, dpi) };
    let _ = AdjustWindowRectExForDpi(&mut rc, style, BOOL(0).into(), exstyle, dpi as u32);
    let (win_w, win_h) = (rc.right - rc.left, rc.bottom - rc.top);

    // Center over the owner (the Settings window) instead of the OS cascade — CW_USEDEFAULT
    // on an owned WS_OVERLAPPED popup dropped this box in the top-left corner. Mirrors
    // `win::run_dialog`'s modal-popup convention; the owner rect is already monitor-correct,
    // so this also keeps About on whatever monitor Settings is on. No DPI conversion needed
    // (screen coords are physical).
    let mut orc = RECT::default();
    let _ = GetWindowRect(parent, &mut orc);
    let x = orc.left + ((orc.right - orc.left) - win_w) / 2;
    let y = orc.top + ((orc.bottom - orc.top) - win_h) / 2;

    if let Ok(hwnd) = CreateWindowExW(
        exstyle,
        class,
        w!("About SageThumbs 2K"),
        style,
        x,
        y,
        win_w,
        win_h,
        Some(parent),
        None,
        Some(hinst),
        None,
    ) {
        if is_dark() {
            dark_control(hwnd, w!("DarkMode_Explorer"));
            dark_titlebar(hwnd);
        }
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
}

// ---- Colour helpers -----------------------------------------------------

fn color_r(c: COLORREF) -> u8 { (c.0 & 0xFF) as u8 }
fn color_g(c: COLORREF) -> u8 { ((c.0 >> 8) & 0xFF) as u8 }
fn color_b(c: COLORREF) -> u8 { ((c.0 >> 16) & 0xFF) as u8 }

unsafe fn s(hwnd: HWND, v: i32) -> i32 {
    dpi_scale(hwnd, v)
}

unsafe fn about_state(hwnd: HWND) -> *mut About {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut About
}

/// The LunarWerx Studios wordmark bytes + aspect ratio for the active theme: the LIGHT
/// (white) variant on the dark card, the DARK (navy) variant on the light card. Picking the
/// matching variant means each is legible composited straight onto its theme background — no
/// dark backing chip.
fn lw_logo() -> (&'static [u8], f32) {
    if is_dark() {
        (LW_LOGO_PNG, 1680.0 / 273.0)
    } else {
        (LW_LOGO_DARK_PNG, 4911.0 / 941.0)
    }
}

/// The themed LunarWerx wordmark sized to `w`×`h`. A STATIC's SS_BITMAP ignores alpha (it
/// BitBlts), so the transparent art is composited onto the card background — seamless in both
/// themes (dark bg in dark mode, light bg in light mode).
unsafe fn lw_logo_hbitmap(w: u32, h: u32) -> Option<HBITMAP> {
    let (bytes, _) = lw_logo();
    let logo = image::load_from_memory(bytes)
        .ok()?
        .resize_exact(w, h, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    let base = DARK_BG();
    let mut out = image::RgbaImage::from_pixel(w, h, image::Rgba([color_r(base), color_g(base), color_b(base), 255]));
    image::imageops::overlay(&mut out, &logo, 0, 0);
    sagethumbs2k_core::app_image::rgba_to_hbitmap(w, h, out.as_raw()).map(|h| HBITMAP(h as *mut c_void))
}

/// The GitHub mark at `px`², tinted `fg` and composited over `fill` (the pill face),
/// so it can be BitBlt'd straight onto the pill with no alpha-blend. The source PNG
/// is a white silhouette whose alpha carries the shape.
unsafe fn github_icon_hbitmap(px: u32, fill: COLORREF, fg: COLORREF) -> Option<HBITMAP> {
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

unsafe fn build_about(hwnd: HWND, hinst: HINSTANCE) {
    // The GitHub mark, built first (composited on the resting pill face) so the very
    // first pill paint already has it.
    let icon_px = s(hwnd, ICON).max(1) as u32;
    let icon = github_icon_hbitmap(icon_px, BTN_FACE(), DARK_TEXT());
    let st = about_state(hwnd);
    if !st.is_null() {
        (*st).gh_icon = icon;
    }

    // Eye logo, centered near the top.
    let logo = ctl(hwnd, STATIC, "", WINDOW_STYLE(SS_BITMAP), (CW - 72) / 2, 20, 72, 72, -1, hinst);
    if let Some(hbmp) = load_art(LOGO_PNG, "logo.png", 72, 72) {
        set_static_bitmap(logo, hbmp);
    }

    // Product title — big + bold — then the muted subtitle.
    let title = ctl(hwnd, STATIC, "SageThumbs 2K", WINDOW_STYLE(SS_CENTER), 20, 100, CW - 40, 34, -1, hinst);
    SendMessageW(title, WM_SETFONT, Some(WPARAM(gui_font_sized(hwnd, 26, 700).0 as usize)), Some(LPARAM(1)));
    ctl(hwnd, STATIC, t("about_subtitle"), WINDOW_STYLE(SS_CENTER), 20, 138, CW - 40, 18, ID_SUBTITLE, hinst);

    // The two status pills, centered as a group. Each pill's width is fixed (the
    // version is constant; the status pill is sized to its widest possible text), so
    // the owner-draw just centers content inside.
    let ver = format!("v{}", env!("CARGO_PKG_VERSION"));
    let ver_w = 14 + ICON + 7 + text_width(&ver) + 14;
    let cand = [
        t("about_checking").to_string(),
        t("about_uptodate").to_string(),
        t("about_check_failed").to_string(),
        format!("{} 99.99.99", t("about_update")),
    ];
    let max_tw = cand.iter().map(|c| text_width(c)).max().unwrap_or(80);
    let status_w = 14 + 10 + 8 + max_tw + 14;
    let gap = 12;
    let gx = (CW - (ver_w + gap + status_w)) / 2;
    let pill = WINDOW_STYLE(SS_OWNERDRAW | SS_NOTIFY);
    ctl(hwnd, STATIC, "", pill, gx, 174, ver_w, 30, ID_VER_PILL, hinst);
    ctl(hwnd, STATIC, "", pill, gx + ver_w + gap, 174, status_w, 30, ID_STATUS_PILL, hinst);

    // Bottom-left: license + copyright (muted via WM_CTLCOLORSTATIC).
    ctl(hwnd, STATIC, "PolyForm Noncommercial 1.0.0", WINDOW_STYLE(0), 22, 250, 210, 16, ID_LICENSE, hinst);
    ctl(hwnd, STATIC, "\u{00a9} 2026 Lunarwerx", WINDOW_STYLE(0), 22, 268, 210, 16, ID_COPYRIGHT, hinst);

    // Bottom-right: the clickable LunarWerx Studios wordmark. The two theme variants have
    // different aspect ratios, so size the control to the active one (fixed height, width
    // from the aspect) — no squish — and right-anchor it.
    let (_, lw_aspect) = lw_logo();
    let lw_h = 26;
    let lw_w = (lw_h as f32 * lw_aspect).round() as i32;
    let lw = ctl(hwnd, STATIC, "", WINDOW_STYLE(SS_BITMAP | SS_NOTIFY), CW - 22 - lw_w, 252, lw_w, lw_h, ID_LW_LOGO, hinst);
    if let Some(hbmp) = lw_logo_hbitmap(lw_w as u32, lw_h as u32) {
        set_static_bitmap(lw, hbmp);
    }
}

// ---- Update check (worker thread → WM_ABOUT_CHECKED) --------------------

/// Kick off a fresh GitHub update check on a worker thread; it posts the outcome
/// back to `hwnd` via [`WM_ABOUT_CHECKED`]. HWND isn't `Send`, so the raw handle
/// value crosses the thread boundary and is rebuilt for the (thread-safe) post.
unsafe fn start_check(hwnd: HWND) {
    let raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let (code, lp) = match update::check() {
            update::UpdateCheck::UpToDate => (0usize, 0isize),
            update::UpdateCheck::Available(tag) => (1usize, Box::into_raw(Box::new(tag)) as isize),
            update::UpdateCheck::Failed => (2usize, 0isize),
        };
        let _ = PostMessageW(
            Some(HWND(raw as *mut c_void)),
            WM_ABOUT_CHECKED,
            WPARAM(code),
            LPARAM(lp),
        );
    });
}

unsafe fn invalidate_status(hwnd: HWND) {
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_STATUS_PILL) {
        let _ = InvalidateRect(Some(h), None, true);
    }
}

/// Kick off an update check with the deliberate ≈2 s "Checking…" animation. The real
/// network probe is near-instant; the spinning ring (and its minimum on-screen time) is the
/// illusion of work — people want to see something move. Guarded by `checking` so a second
/// click while it runs is a no-op.
unsafe fn begin_check(hwnd: HWND) {
    let st = about_state(hwnd);
    if st.is_null() {
        return;
    }
    (*st).checking = true;
    (*st).status = Status::Checking;
    (*st).pending = None;
    (*st).spin_frame = 0;
    let _ = SetTimer(Some(hwnd), SPIN_TIMER_ID, SPIN_INTERVAL_MS, None);
    invalidate_status(hwnd);
    start_check(hwnd);
}

/// Commit a finished check to the pill and stop the spinner.
unsafe fn reveal(hwnd: HWND, result: Status) {
    let st = about_state(hwnd);
    if st.is_null() {
        return;
    }
    let _ = KillTimer(Some(hwnd), SPIN_TIMER_ID);
    (*st).checking = false;
    (*st).pending = None;
    (*st).status = result;
    invalidate_status(hwnd);
}

/// The status pill was clicked while an update is available: offer the same one-click,
/// in-place update the Settings button used to (download → verify → elevated install),
/// falling back to the releases page if it can't complete.
unsafe fn offer_update(hwnd: HWND) {
    let cap = wide("Update SageThumbs 2K");
    let prompt = wide(
        "Download and install the update now? SageThumbs updates itself in the background — Explorer briefly restarts, and you'll get a confirmation when it's done.",
    );
    if MessageBoxW(Some(hwnd), PCWSTR(prompt.as_ptr()), PCWSTR(cap.as_ptr()), MB_YESNO | MB_ICONINFORMATION) != IDYES {
        return;
    }
    match update::download_and_install(hwnd) {
        // Installer launched: it closes us, upgrades in place, and relaunches — so exit.
        Ok(_) => std::process::exit(0),
        // A user cancel (progress-dialog Cancel or declining UAC) shouldn't nag.
        Err(m) if m.contains("cancel") => {}
        // A real failure: fall back to the manual download page.
        Err(_) => open_url(update::RELEASES_URL),
    }
}

/// Status-pill click: install a waiting update, otherwise re-run the check (unless one is
/// already in flight).
unsafe fn on_status_click(hwnd: HWND) {
    let st = about_state(hwnd);
    if st.is_null() {
        return;
    }
    if let Status::Available(_) = (*st).status {
        offer_update(hwnd);
        return;
    }
    if (*st).checking {
        return;
    }
    begin_check(hwnd);
}

// ---- Owner-draw ---------------------------------------------------------

/// Text extent of `text` in the HDC's currently-selected font.
unsafe fn measure(hdc: HDC, text: &str) -> i32 {
    let w = wide(text);
    let n = w.len().saturating_sub(1);
    let mut sz = SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &w[..n], &mut sz);
    sz.cx
}

unsafe fn fill_rc(hdc: HDC, rc: &RECT, color: COLORREF) {
    SetDCBrushColor(hdc, color);
    FillRect(hdc, rc, HBRUSH(GetStockObject(DC_BRUSH).0));
}

/// Paint the rounded pill frame (face + hairline border) into `rc` — full-stadium
/// rounding (ellipse == height).
unsafe fn pill_frame(hwnd: HWND, hdc: HDC, rc: &RECT) {
    SelectObject(hdc, GetStockObject(DC_BRUSH));
    SelectObject(hdc, GetStockObject(DC_PEN));
    SetDCBrushColor(hdc, BTN_FACE());
    SetDCPenColor(hdc, BORDER_STRONG());
    let h = rc.bottom - rc.top;
    let inset = s(hwnd, 1);
    let _ = RoundRect(hdc, rc.left, rc.top, rc.right - inset, rc.bottom - inset, h, h);
}

/// Blit an opaque bitmap into `dst` at `(x,y)`, `w`×`h`.
unsafe fn blit(dst: HDC, hbmp: HBITMAP, x: i32, y: i32, w: i32, h: i32) {
    let mdc = CreateCompatibleDC(Some(dst));
    if mdc.is_invalid() {
        return;
    }
    let old = SelectObject(mdc, HGDIOBJ(hbmp.0));
    let _ = BitBlt(dst, x, y, w, h, Some(mdc), 0, 0, SRCCOPY);
    SelectObject(mdc, old);
    let _ = DeleteDC(mdc);
}

/// Draw text left-aligned + vertically centered starting at `left`.
unsafe fn draw_text(hdc: HDC, text: &str, left: i32, rc: &RECT, color: COLORREF) {
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, color);
    let mut buf = wide(text);
    let n = buf.len().saturating_sub(1);
    let mut tr = RECT { left, top: rc.top, right: rc.right, bottom: rc.bottom };
    DrawTextW(hdc, &mut buf[..n], &mut tr, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
}

unsafe fn draw_ver_pill(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    fill_rc(hdc, &rc, DARK_BG());
    pill_frame(hwnd, hdc, &rc);

    let icon_px = s(hwnd, ICON);
    let gap = s(hwnd, 7);
    let ver = format!("v{}", env!("CARGO_PKG_VERSION"));
    SelectObject(hdc, HGDIOBJ(gui_font_for(hwnd).0));
    let tw = measure(hdc, &ver);
    let group = icon_px + gap + tw;
    let gx = rc.left + ((rc.right - rc.left) - group) / 2;
    let iy = rc.top + ((rc.bottom - rc.top) - icon_px) / 2;
    let st = about_state(hwnd);
    if !st.is_null() {
        if let Some(icon) = (*st).gh_icon {
            blit(hdc, icon, gx, iy, icon_px, icon_px);
        }
    }
    draw_text(hdc, &ver, gx + icon_px + gap, &rc, DARK_TEXT());
}

/// Map the current status to (dot colour, label).
unsafe fn status_display(st: *mut About) -> (COLORREF, String) {
    if st.is_null() {
        return (rgb(150, 150, 150), t("about_checking").to_string());
    }
    match &(*st).status {
        Status::Checking => (rgb(150, 150, 150), t("about_checking").to_string()),
        Status::UpToDate => (rgb(63, 185, 80), t("about_uptodate").to_string()),
        Status::Available(tag) => (rgb(210, 153, 34), format!("{} {}", t("about_update"), tag)),
        Status::Failed => (rgb(190, 110, 110), t("about_check_failed").to_string()),
    }
}

/// A rotating 270° arc (a classic loading ring) centered at `(cx,cy)`, radius `r`, oriented
/// by `frame` so successive repaints appear to spin. The two radial endpoints only pick the
/// sweep, so the exact angle-sign convention doesn't matter — either direction reads as
/// "spinning". Uses a DPI-scaled pen freed before returning.
unsafe fn draw_spinner(hwnd: HWND, hdc: HDC, cx: i32, cy: i32, r: i32, frame: u32, color: COLORREF) {
    use core::f32::consts::PI;
    let pen_w = s(hwnd, 2).max(1);
    let pen = CreatePen(PS_SOLID, pen_w, color);
    if pen.is_invalid() {
        return;
    }
    let old = SelectObject(hdc, HGDIOBJ(pen.0));
    let t0 = (frame as f32) * 12.0 * PI / 180.0; // ~12°/frame → a smooth, clearly visible spin
    let t1 = t0 + 270.0 * PI / 180.0; // a gapped ring, not a closed circle
    let (rf, cxf, cyf) = (r as f32, cx as f32, cy as f32);
    let sx = (cxf + rf * t0.cos()).round() as i32;
    let sy = (cyf - rf * t0.sin()).round() as i32;
    let ex = (cxf + rf * t1.cos()).round() as i32;
    let ey = (cyf - rf * t1.sin()).round() as i32;
    let _ = Arc(hdc, cx - r, cy - r, cx + r, cy + r, sx, sy, ex, ey);
    SelectObject(hdc, old);
    let _ = DeleteObject(HGDIOBJ(pen.0));
}

unsafe fn draw_status_pill(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    fill_rc(hdc, &rc, DARK_BG());
    pill_frame(hwnd, hdc, &rc);

    let st = about_state(hwnd);
    let (dot, text) = status_display(st);
    let checking = !st.is_null() && matches!((*st).status, Status::Checking);
    let frame = if st.is_null() { 0 } else { (*st).spin_frame };
    let dotd = s(hwnd, 10);
    let gap = s(hwnd, 8);
    SelectObject(hdc, HGDIOBJ(gui_font_for(hwnd).0));
    let tw = measure(hdc, &text);
    let group = dotd + gap + tw;
    let gx = rc.left + ((rc.right - rc.left) - group) / 2;
    let dy = rc.top + ((rc.bottom - rc.top) - dotd) / 2;
    if checking {
        // Spinning ring in the dot's slot — the moving "faux" activity while we check.
        let r = (dotd / 2 - s(hwnd, 1)).max(2);
        draw_spinner(hwnd, hdc, gx + dotd / 2, dy + dotd / 2, r, frame, dot);
    } else {
        // Resting status dot.
        SelectObject(hdc, GetStockObject(DC_BRUSH));
        SelectObject(hdc, GetStockObject(DC_PEN));
        SetDCBrushColor(hdc, dot);
        SetDCPenColor(hdc, dot);
        let _ = Ellipse(hdc, gx, dy, gx + dotd, dy + dotd);
    }
    draw_text(hdc, &text, gx + dotd + gap, &rc, DARK_TEXT());
}

unsafe fn ctlcolor_text(hdc: HDC, color: COLORREF) -> LRESULT {
    SetTextColor(hdc, color);
    SetBkColor(hdc, DARK_BG());
    SetBkMode(hdc, TRANSPARENT);
    LRESULT(dark_bg_brush().0 as isize)
}

extern "system" fn about_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        // Muted on-surface colours for the subtitle / license / copyright — handled
        // BEFORE the generic static colouring so they don't get the default text colour.
        if msg == WM_CTLCOLORSTATIC {
            let id = GetDlgCtrlID(HWND(lparam.0 as *mut c_void));
            let hdc = HDC(wparam.0 as *mut c_void);
            let muted = match id {
                ID_SUBTITLE | ID_LICENSE => Some(HEADER_TEXT()),
                ID_COPYRIGHT => Some(DISABLED_TEXT()),
                _ => None,
            };
            if let Some(c) = muted {
                return ctlcolor_text(hdc, c);
            }
        }
        if let Some(r) = dark_ctlcolor(msg, wparam) {
            return r;
        }
        match msg {
            WM_CREATE => {
                let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
                let state = Box::new(About {
                    status: Status::Checking,
                    checking: false,
                    spin_frame: 0,
                    pending: None,
                    gh_icon: None,
                });
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
                build_about(hwnd, hinst);
                begin_check(hwnd); // check on open, with the ≈2 s spinner
                LRESULT(0)
            }
            WM_DRAWITEM => {
                let d = &*(lparam.0 as *const DRAWITEMSTRUCT);
                match d.CtlID as i32 {
                    ID_VER_PILL => draw_ver_pill(hwnd, d),
                    ID_STATUS_PILL => draw_status_pill(hwnd, d),
                    _ => {}
                }
                LRESULT(1)
            }
            WM_ABOUT_CHECKED => {
                let st = about_state(hwnd);
                if st.is_null() {
                    if lparam.0 != 0 {
                        // Window torn down between post and dispatch — reclaim the tag.
                        drop(Box::from_raw(lparam.0 as *mut String));
                    }
                    return LRESULT(0);
                }
                let result = match wparam.0 {
                    1 => {
                        let tag = if lparam.0 != 0 {
                            *Box::from_raw(lparam.0 as *mut String)
                        } else {
                            String::new()
                        };
                        Status::Available(tag)
                    }
                    2 => Status::Failed,
                    _ => Status::UpToDate,
                };
                // Faux timer: if the spinner hasn't run for its minimum yet, hold the result
                // and let WM_TIMER reveal it once ≈2 s has passed; otherwise show it now.
                if (*st).spin_frame >= MIN_SPIN_FRAMES {
                    reveal(hwnd, result);
                } else {
                    (*st).pending = Some(result);
                }
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == SPIN_TIMER_ID => {
                let st = about_state(hwnd);
                if !st.is_null() {
                    (*st).spin_frame = (*st).spin_frame.saturating_add(1);
                    if (*st).spin_frame >= MIN_SPIN_FRAMES {
                        if let Some(result) = (*st).pending.take() {
                            reveal(hwnd, result); // min time met and result ready → show + stop
                            return LRESULT(0);
                        }
                    }
                    invalidate_status(hwnd); // advance the spinner one frame
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                let notify = ((wparam.0 >> 16) & 0xFFFF) as u32;
                match id {
                    IDOK | IDCANCEL => {
                        let _ = DestroyWindow(hwnd);
                    }
                    ID_LW_LOGO if notify == STN_CLICKED => open_url(URL_PARENT),
                    ID_VER_PILL if notify == STN_CLICKED => open_url(URL_GITHUB),
                    ID_STATUS_PILL if notify == STN_CLICKED => on_status_click(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_SETCURSOR => {
                // Hand cursor over the three clickables; default elsewhere.
                let over = HWND(wparam.0 as *mut c_void);
                let clickable = [ID_LW_LOGO, ID_VER_PILL, ID_STATUS_PILL]
                    .iter()
                    .any(|&id| GetDlgItem(Some(hwnd), id).map(|h| h == over).unwrap_or(false));
                if clickable {
                    if let Ok(hand) = LoadCursorW(None, IDC_HAND) {
                        SetCursor(Some(hand));
                    }
                    return LRESULT(1);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_DPICHANGED => {
                wm_dpichanged(hwnd, lparam);
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                let p = about_state(hwnd);
                if !p.is_null() {
                    let _ = KillTimer(Some(hwnd), SPIN_TIMER_ID);
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    let st = Box::from_raw(p);
                    if let Some(icon) = st.gh_icon {
                        let _ = DeleteObject(HGDIOBJ(icon.0));
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
