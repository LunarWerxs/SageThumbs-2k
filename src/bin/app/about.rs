//! The About box — the "2026" card.
//!
//! A compact, owner-drawn popup that mirrors the product mock: the eye logo, the
//! product title + subtitle, two clickable status *pills* (a GitHub version chip
//! that opens the repo, and a live "Up to date" update-check chip), the license /
//! copyright in the bottom-left, and the clickable LunarWerx Studios wordmark in
//! the bottom-right. The update check runs on a worker thread when the box opens
//! and again whenever the user clicks the status pill, so the chip is never stale.

use core::ffi::c_void;
mod paint;
use paint::*;
mod checker;
use checker::*;
mod build;
use build::*;
#[cfg(test)]
mod tests;

use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    Arc, BitBlt, CreateCompatibleDC, CreatePen, DeleteDC, DeleteObject, DrawTextW, Ellipse,
    FillRect, GetStockObject, GetTextExtentPoint32W, InvalidateRect, RoundRect, SelectObject,
    SetBkColor, SetBkMode, SetDCBrushColor, SetDCPenColor, SetTextColor, DC_BRUSH, DC_PEN, DT_LEFT,
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
    ctl, dpi_scale, dpi_scale_dpi, gui_font_for, gui_font_sized, load_art, open_url,
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
/// The "Send feedback" pill — same owner-drawn stadium as the version/status pills,
/// centered on its own row in the band between them and the footer.
const ID_FEEDBACK_PILL: i32 = 1206;
/// The licence-state line ("Licensed, last verified …" / "No licence key entered" /
/// "Licence revoked (key …)" / "Personal use, no licence needed") — the SAME text the
/// Settings page's Licence status line shows, from the same shared formatter
/// (`settings_dlg::licence_state_line`). Not `ID_LICENSE` — that one names the software
/// LICENSE this app ships under ("PolyForm Noncommercial 1.0.0"), an unrelated fact.
const ID_LICENCE_STATE: i32 = 1207;

/// Posted from the update-check worker thread back to the About window: the check
/// finished. `WPARAM` = outcome (0 up-to-date, 1 update available, 2 failed); `LPARAM` is
/// unused. The found release travels in [`checker::FOUND_RELEASE`], never in the message.
const WM_ABOUT_CHECKED: u32 = WM_APP + 1;

/// Posted from the download+install worker thread back to the About window: the
/// one-click update attempt finished. `LPARAM` is
/// `Box::into_raw(Box<Result<String, update::UpdateError>>)` — the handler reclaims it.
/// `WPARAM` is unused. See [`start_install`]: this replaces calling
/// `update::download_and_install` directly on the UI thread, which used to block the
/// whole message loop (and everything else on it, including Settings behind it) for
/// the entire multi-MB download.
const WM_ABOUT_INSTALLED: u32 = WM_APP + 2;

/// Timer id driving the status-pill spinner animation.
const SPIN_TIMER_ID: usize = 1;
/// Spinner repaint interval (ms) — ~25 fps: smooth motion, negligible cost.
const SPIN_INTERVAL_MS: u32 = 40;
/// Minimum frames the "Checking…" spinner stays up before the result is shown — a
/// deliberate ≈2 s illusion of work, since the real check is near-instant. 50 × 40 ms ≈ 2 s.
const MIN_SPIN_FRAMES: u32 = 50;

/// Client size in 96-DPI design pixels (DPI-scaled per control / for the frame).
const CW: i32 = 440;
/// 318, not the original 300: the bottom-left block grew a third line (the licence-state
/// line, between the software-licence line and the copyright line) when the Licence page
/// landed, so the card needed the extra ~18px to keep the same margin below it.
const CH: i32 = 318;

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
    /// Automatic checking is switched OFF and nobody has pressed anything, so we have not
    /// looked and must not imply that we have. The pill still invites a manual check.
    Idle,
    Checking,
    UpToDate,
    /// A newer release exists. Carries the WHOLE release, not just its tag: the pill only
    /// draws the tag, but the click handler needs the publication date and the
    /// security-release flag to decide whether this machine's updates window covers it
    /// (`update::update_offer`), and re-fetching them on the click would be a second network
    /// round trip for facts the check already had in hand.
    Available(update::LatestRelease),
    Failed,
}

/// Per-window state, owned via `GWLP_USERDATA`.
struct About {
    status: Status,
    /// A network check is in flight — ignore extra status-pill clicks until it lands.
    checking: bool,
    /// A download+install attempt is in flight on a worker thread — ignore extra
    /// status-pill clicks until [`WM_ABOUT_INSTALLED`] lands (or the process exits,
    /// on the success path).
    installing: bool,
    /// Spinner animation phase; doubles as the elapsed-frame counter for the faux timer.
    spin_frame: u32,
    /// A finished check whose result is held back until the faux timer's minimum elapses.
    pending: Option<Status>,
    /// The GitHub mark, pre-composited on the pill fill so the blit is seamless. Freed
    /// in `WM_NCDESTROY`.
    gh_icon: Option<HBITMAP>,
    /// The product logo and the LunarWerx wordmark handed to STATIC controls via
    /// `STM_SETIMAGE`. A STATIC does NOT free an image set that way, and `set_static_bitmap`
    /// only deletes whatever the control held BEFORE (nothing, on a fresh control) — so
    /// without keeping these they leaked one HBITMAP each per About open/close, exactly the
    /// bug `gh_icon` was already tracked to avoid. Freed in `WM_NCDESTROY`.
    logo_icon: Option<HBITMAP>,
    lw_icon: Option<HBITMAP>,
}

/// Open the About box, owned by `parent`.
pub(crate) unsafe fn show_about(parent: HWND) {
    let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
    let class = w!("SageThumbs2KAbout");
    // Theme-aware background, app icon, arrow cursor; idempotent on a second call.
    crate::win::register_app_class(class, Some(about_wndproc), hinst);

    // Size the frame so the *client* area is exactly the design size, scaled to the
    // parent's DPI (identity at 96 → standard displays are unchanged).
    let dpi = GetDpiForWindow(parent) as i32;
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
    let exstyle = WS_EX_DLGMODALFRAME;
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: dpi_scale_dpi(CW, dpi),
        bottom: dpi_scale_dpi(CH, dpi),
    };
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

/// Headless capture of the About box (`--shot <out.png> --window about`) — built
/// off-screen and `PrintWindow`ed, so the pills (including "Send feedback") are
/// verifiable without opening a window or driving the desktop.
///
/// `build_about` lays out against ABSOLUTE design coords assuming the client is
/// exactly `CW`×`CH`, but `create_shot_window`'s `design_w/h` size the whole WINDOW
/// — so the frame is added back here rather than guessed at the call.
pub(crate) unsafe fn run_shot_about(out: &str) -> bool {
    crate::win::capture_shot_window(
        out,
        is_dark(),
        crate::win::ShotWindowSpec {
            class: w!("SageThumbs2KAbout"),
            wndproc: Some(about_wndproc),
            title: "About SageThumbs 2K",
            design_w: CW,
            design_h: CH,
        },
        |hwnd, _hinst| unsafe {
            // Grow the frame so the CLIENT is the design size the controls were placed against.
            let dpi = GetDpiForWindow(hwnd).max(96) as i32;
            let mut rc = RECT {
                left: 0,
                top: 0,
                right: dpi_scale_dpi(CW, dpi),
                bottom: dpi_scale_dpi(CH, dpi),
            };
            let style = WINDOW_STYLE(GetWindowLongW(hwnd, GWL_STYLE) as u32);
            let exstyle = WINDOW_EX_STYLE(GetWindowLongW(hwnd, GWL_EXSTYLE) as u32);
            let _ = AdjustWindowRectExForDpi(&mut rc, style, BOOL(0).into(), exstyle, dpi as u32);
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                rc.right - rc.left,
                rc.bottom - rc.top,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        },
        // Pump past MIN_SPIN_FRAMES (≈2 s) so the status pill settles on a real result
        // instead of catching the deliberate "Checking…" spinner mid-animation.
        140,
        8,
        false,
    )
}

// ---- Colour helpers -----------------------------------------------------

fn color_r(c: COLORREF) -> u8 {
    (c.0 & 0xFF) as u8
}
fn color_g(c: COLORREF) -> u8 {
    ((c.0 >> 8) & 0xFF) as u8
}
fn color_b(c: COLORREF) -> u8 {
    ((c.0 >> 16) & 0xFF) as u8
}

unsafe fn s(hwnd: HWND, v: i32) -> i32 {
    dpi_scale(hwnd, v)
}

unsafe fn about_state(hwnd: HWND) -> *mut About {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut About
}

// ---- Update check (worker thread → WM_ABOUT_CHECKED) --------------------

// ---- Owner-draw ---------------------------------------------------------

unsafe fn on_create(hwnd: HWND) -> LRESULT {
    let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
    // Opening a page is not a request to hit the network. If the user has turned
    // "Automatically check for updates" off, respect it here too — this arm used to
    // fire regardless, which is exactly what issue #26 reported. The manual pill
    // click below still checks unconditionally, because that IS a request.
    let auto = sagethumbs2k_core::settings::update_auto_check();
    let state = Box::new(About {
        status: if auto { Status::Checking } else { Status::Idle },
        checking: false,
        installing: false,
        spin_frame: 0,
        pending: None,
        gh_icon: None,
        logo_icon: None,
        lw_icon: None,
    });
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
    build_about(hwnd, hinst);
    if auto {
        begin_check(hwnd); // check on open, with the ≈2 s spinner
    }
    LRESULT(0)
}

unsafe fn on_drawitem(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let d = &*(lparam.0 as *const DRAWITEMSTRUCT);
    match d.CtlID as i32 {
        ID_VER_PILL => draw_ver_pill(hwnd, d),
        ID_STATUS_PILL => draw_status_pill(hwnd, d),
        ID_FEEDBACK_PILL => draw_feedback_pill(hwnd, d),
        _ => {}
    }
    LRESULT(1)
}

unsafe fn on_about_checked(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    let st = about_state(hwnd);
    if st.is_null() {
        return LRESULT(0);
    }
    let result = status_for_code(wparam.0);
    // Faux timer: if the spinner hasn't run for its minimum yet, hold the result
    // and let WM_TIMER reveal it once ≈2 s has passed; otherwise show it now.
    if (*st).spin_frame >= MIN_SPIN_FRAMES {
        reveal(hwnd, result);
    } else {
        (*st).pending = Some(result);
    }
    LRESULT(0)
}

unsafe fn on_spin_timer(hwnd: HWND) -> LRESULT {
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

unsafe fn on_command(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    let (id, notify) = crate::win::command_parts(wparam);
    match id {
        IDOK | IDCANCEL => {
            let _ = DestroyWindow(hwnd);
        }
        ID_LW_LOGO if notify == STN_CLICKED => open_url(URL_PARENT),
        ID_VER_PILL if notify == STN_CLICKED => open_url(URL_GITHUB),
        ID_STATUS_PILL if notify == STN_CLICKED => on_status_click(hwnd),
        // Modal to About, so the box the user was reading stays put behind it.
        ID_FEEDBACK_PILL if notify == STN_CLICKED => crate::feedback::show_feedback(hwnd),
        _ => {}
    }
    LRESULT(0)
}

unsafe fn on_setcursor(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Hand cursor over the four clickables; default elsewhere.
    let over = HWND(wparam.0 as *mut c_void);
    let clickable = [ID_LW_LOGO, ID_VER_PILL, ID_STATUS_PILL, ID_FEEDBACK_PILL]
        .iter()
        .any(|&id| {
            GetDlgItem(Some(hwnd), id)
                .map(|h| h == over)
                .unwrap_or(false)
        });
    if clickable {
        if let Ok(hand) = LoadCursorW(None, IDC_HAND) {
            SetCursor(Some(hand));
        }
        return LRESULT(1);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe fn on_ncdestroy(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let p = about_state(hwnd);
    if !p.is_null() {
        let _ = KillTimer(Some(hwnd), SPIN_TIMER_ID);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        let st = Box::from_raw(p);
        for icon in [st.gh_icon, st.logo_icon, st.lw_icon].into_iter().flatten() {
            let _ = DeleteObject(HGDIOBJ(icon.0));
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

extern "system" fn about_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if let Some(r) = themed_ctlcolor(msg, wparam, lparam) {
            return r;
        }
        match msg {
            WM_CREATE => on_create(hwnd),
            WM_DRAWITEM => on_drawitem(hwnd, lparam),
            WM_ABOUT_CHECKED => on_about_checked(hwnd, wparam),
            WM_ABOUT_INSTALLED => on_about_installed(hwnd, lparam),
            WM_TIMER if wparam.0 == SPIN_TIMER_ID => on_spin_timer(hwnd),
            WM_COMMAND => on_command(hwnd, wparam),
            WM_SETCURSOR => on_setcursor(hwnd, msg, wparam, lparam),
            WM_DPICHANGED => {
                wm_dpichanged(hwnd, lparam);
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_NCDESTROY => on_ncdestroy(hwnd, msg, wparam, lparam),
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Theme-aware colour queries for the About box's children: answers `WM_CTLCOLORSTATIC`
/// (muted subtitle / license / copyright first, so they beat the generic colouring) and
/// the dark-mode control colours, returning `Some` so the window procedure returns it verbatim.
unsafe fn themed_ctlcolor(msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    // Muted on-surface colours for the subtitle / license / copyright — handled
    // BEFORE the generic static colouring so they don't get the default text colour.
    if msg == WM_CTLCOLORSTATIC {
        if let Some(r) = muted_static_color(wparam, lparam) {
            return Some(r);
        }
    }
    if let Some(r) = dark_ctlcolor(msg, wparam) {
        return Some(r);
    }
    None
}
