//! The About box.
//!
//! A small popup: logo, version, the company links (SysLink), a tagline, and the
//! clickable LunarWerx wordmark. The richer home for the promotion; the main
//! Settings dialog keeps just the footer link.

use core::ffi::c_void;

use windows::core::{w, BOOL};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::HBITMAP;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{NMHDR, NMLINK, NM_CLICK, NM_RETURN};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::{dark_bg_brush, dark_control, dark_ctlcolor, dark_titlebar, is_dark};
use crate::win::{
    app_icon, ctl, dpi_scale_dpi, load_art, open_url, set_static_bitmap, t, text_width,
    wm_dpichanged, wstr_to_string, BUTTON, STATIC, SYSLINK, SS_BITMAP, SS_CENTER, SS_NOTIFY,
    IDCANCEL, IDOK, URL_COMPANIES, URL_GITHUB,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};

// Company promotion (the About box's clickable controls).
const ID_ABOUT_LINK: i32 = 1121;
/// The clickable LunarWerx wordmark in the About box.
const ID_LW_LOGO: i32 = 1119;

/// Logo artwork, embedded so it always renders. A `logo.png` next to the EXE
/// overrides at runtime (user-swappable).
const LOGO_PNG: &[u8] = include_bytes!("../../../assets/logo.png");
/// LunarWerx wordmark (white-on-transparent, 1680×273) for the About box.
const LW_LOGO_PNG: &[u8] = include_bytes!("../../../assets/lw_logo_white.png");

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

    // Scale the frame to the parent's DPI so the (ctl-scaled) controls fit at >96
    // DPI. Identity at 96 (dpi_scale_dpi(v, 96) == v) → no change on a standard display.
    let dpi = GetDpiForWindow(parent) as i32;
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
    let exstyle = WS_EX_DLGMODALFRAME;
    // Size the frame so the *client* area is exactly the design size (400×422
    // scaled). The controls center on a 400-wide client; creating the window at
    // 400 wide instead left the client ~frame px narrower, so everything sat
    // right-of-center. AdjustWindowRectExForDpi adds the frame back.
    let mut rc = RECT { left: 0, top: 0, right: dpi_scale_dpi(400, dpi), bottom: dpi_scale_dpi(446, dpi) };
    let _ = AdjustWindowRectExForDpi(&mut rc, style, BOOL(0).into(), exstyle, dpi as u32);
    if let Ok(hwnd) = CreateWindowExW(
        exstyle,
        class,
        w!("About SageThumbs 2K"),
        style,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        rc.right - rc.left,
        rc.bottom - rc.top,
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

/// The LunarWerx wordmark sized to `w`×`h`. The art is white-on-transparent, so
/// in LIGHT mode it's composited onto a dark chip first (it would otherwise be
/// invisible on the pale dialog); in dark mode the transparency is kept.
unsafe fn lw_logo_hbitmap(w: u32, h: u32) -> Option<HBITMAP> {
    let logo = image::load_from_memory(LW_LOGO_PNG)
        .ok()?
        .resize_exact(w, h, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    let rgba = if is_dark() {
        logo
    } else {
        let mut chip = image::RgbaImage::from_pixel(w, h, image::Rgba([43, 43, 43, 255]));
        image::imageops::overlay(&mut chip, &logo, 0, 0);
        chip
    };
    sagethumbs2k::app_image::rgba_to_hbitmap(w, h, rgba.as_raw()).map(|h| HBITMAP(h as *mut c_void))
}

unsafe fn build_about(hwnd: HWND, hinst: HINSTANCE) {
    let logo = ctl(hwnd, STATIC, "", WINDOW_STYLE(SS_BITMAP), 164, 18, 72, 72, -1, hinst);
    if let Some(hbmp) = load_art(LOGO_PNG, "logo.png", 72, 72) {
        set_static_bitmap(logo, hbmp);
    }
    let center = WINDOW_STYLE(SS_CENTER);
    ctl(hwnd, STATIC, "SageThumbs 2K", center, 20, 100, 360, 22, -1, hinst);
    let ver = format!("{} {}", t("about_version"), env!("CARGO_PKG_VERSION"));
    ctl(hwnd, STATIC, &ver, center, 20, 124, 360, 18, -1, hinst);
    ctl(hwnd, STATIC, t("about_desc"), center, 20, 150, 360, 18, -1, hinst);
    // Center the repo link: measure the visible text and place the SysLink rect so
    // it sits in the middle (SysLink left-aligns its text within its own rect).
    let visible = "github.com/LunarWerxs/SageThumbs-2k";
    let tw = text_width(visible);
    let lx = ((400 - tw) / 2).max(8);
    let link = format!("<a href=\"{URL_GITHUB}\">{visible}</a>");
    ctl(hwnd, SYSLINK, &link, WINDOW_STYLE(0), lx, 184, tw + 8, 20, ID_ABOUT_LINK, hinst);
    ctl(hwnd, STATIC, t("about_tagline"), center, 20, 216, 360, 34, -1, hinst);
    // The LunarWerx wordmark, below the tagline and above Close; clicking it
    // opens the companies page (SS_NOTIFY → STN_CLICKED; hand cursor in wndproc).
    // Tuned size/spacing: 231×38 (25% down from 308×50), 30px above Close.
    let lw = ctl(hwnd, STATIC, "", WINDOW_STYLE(SS_BITMAP | SS_NOTIFY), 84, 258, 231, 38, ID_LW_LOGO, hinst);
    if let Some(hbmp) = lw_logo_hbitmap(231, 38) {
        set_static_bitmap(lw, hbmp);
    }
    // Credit the original author + show the license/copyright. The project is a clean-room
    // rewrite — the SageThumbs name is Nikolay Raspopov's — so we credit him in the About
    // box as well as the README. (Kept in English: proper nouns + a license name.)
    ctl(hwnd, STATIC, "A clean-room rewrite of SageThumbs by Nikolay Raspopov.", center, 20, 300, 360, 16, -1, hinst);
    ctl(hwnd, STATIC, "PolyForm Noncommercial 1.0.0  \u{00b7}  \u{00a9} 2026 Lunarwerx", center, 20, 320, 360, 16, -1, hinst);
    ctl(hwnd, BUTTON, t("btn_close"), WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP, 158, 352, 84, 28, IDOK, hinst);
}

extern "system" fn about_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if let Some(r) = dark_ctlcolor(msg, wparam) {
            return r;
        }
        match msg {
            WM_CREATE => {
                let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
                build_about(hwnd, hinst);
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                if id == IDOK || id == IDCANCEL {
                    let _ = DestroyWindow(hwnd);
                } else if id == ID_LW_LOGO {
                    open_url(URL_COMPANIES); // the wordmark is a link (STN_CLICKED)
                }
                LRESULT(0)
            }
            WM_SETCURSOR => {
                // Hand cursor over the clickable wordmark; everything else default.
                let over = HWND(wparam.0 as *mut c_void);
                if GetDlgItem(Some(hwnd), ID_LW_LOGO).map(|h| h == over).unwrap_or(false) {
                    if let Ok(hand) = LoadCursorW(None, IDC_HAND) {
                        SetCursor(Some(hand));
                    }
                    return LRESULT(1);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_NOTIFY => {
                let nmhdr = lparam.0 as *const NMHDR;
                if (*nmhdr).code == NM_CLICK || (*nmhdr).code == NM_RETURN {
                    let link = lparam.0 as *const NMLINK;
                    let url = wstr_to_string(&(*link).item.szUrl);
                    if !url.is_empty() {
                        open_url(&url);
                    }
                }
                LRESULT(0)
            }
            WM_DPICHANGED => {
                wm_dpichanged(hwnd, lparam);
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
