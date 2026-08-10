//! The **first-run welcome** — one small window, shown once, that offers the two
//! features a fresh install leaves switched OFF: Quick preview and the screenshot hotkey.
//!
//! Both are deliberately opt-in, which is correct for a shell extension but meant most
//! users never discovered them: nothing in Explorer hints that Space previews a file, and
//! the capture hotkey is invisible until you go looking in Settings. Asking once, at the
//! moment the app first opens, is the cheapest way to surface them without turning
//! anything on behind the user's back.
//!
//! One window rather than two sequential yes/no prompts: the PrtScn choice is a *dependent*
//! of the screenshot answer, so it has to be able to grey out (it does — see `sync_prtscn`),
//! and two modal boxes in a row on first launch reads as nagging.
//!
//! Shown ONCE, tracked by the `FirstRunShown` HKCU flag, which is written whichever way the
//! window is dismissed — closing with the X must not mean "ask me again tomorrow".

use core::ffi::c_void;

use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::{dark_ctlcolor, dark_ctlcolor_dim};
use crate::win::{
    check, checked, ctl, dpi_scale, run_dialog, t, wm_dpichanged, BUTTON, IDOK, STATIC,
};

/// HKCU flag: has the welcome window been shown? Written on ANY dismissal.
const FIRST_RUN_SHOWN: &str = "FirstRunShown";

/// Bare PrtScn with no modifier, in the packed HOTKEYF/VK form `settings` stores.
/// (`DEFAULT_SHOT_HOTKEY` is the same VK with HOTKEYF_CONTROL in the high byte.)
const PRTSCN_ONLY: u32 = 0x2C; // VK_SNAPSHOT, no modifiers

/// Windows 11's own "Print screen opens Snipping Tool" binding. While it is 1 the shell
/// swallows the key before any `RegisterHotKey`, so a user who asks for bare PrtScn would
/// get a hotkey that silently never fires. Turning it off is the whole point of that
/// checkbox, and the checkbox says so.
const SNIP_KEY_PATH: &str = r"Control Panel\Keyboard";
const SNIP_KEY_VALUE: &str = "PrintScreenKeyForSnippingEnabled";

const ID_HEAD: i32 = 100;
const ID_PREVIEW: i32 = 101;
const ID_PREVIEW_SUB: i32 = 102;
const ID_SHOT: i32 = 103;
const ID_SHOT_SUB: i32 = 104;
const ID_PRTSCN: i32 = 105;
const ID_THUMBS: i32 = 106;
const ID_THUMBS_SUB: i32 = 107;
// Page 2 — two more opt-ins, the SAME shape as page 1: a switch that names the behavior,
// a muted line saying what it does. (An earlier draft asked "what do you keep in your
// folders" with persona radios; review verdict: nobody can answer that. Ask the direct
// question instead.)
const ID_P2_HEAD: i32 = 110;
const ID_P_COVERS: i32 = 111;
const ID_P_COVERS_SUB: i32 = 112;
const ID_P_SCANLATION: i32 = 113;
const ID_P_SCANLATION_SUB: i32 = 114;
const ID_P2_SUB: i32 = 115;

// Which page the single window is showing. One window that swaps content, not two modal
// boxes in a row — the module doc's "reads as nagging" rule is why.
thread_local! {
    static ON_PAGE_2: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

const DLG_W: i32 = 460;
const DLG_H: i32 = 340;
/// Extra height the portable-only thumbnails row needs (checkbox + its two-line caption).
const THUMBS_ROW_H: i32 = 68;

/// Does this copy get the thumbnails row? Only a portable one: an installed build registered
/// the handler machine-wide at setup, so offering it again would be a switch that does nothing.
fn offers_thumbnails() -> bool {
    sagethumbs2k_core::settings::portable()
}

/// Window height — the portable build carries one extra row.
fn dlg_h() -> i32 {
    if offers_thumbnails() {
        DLG_H + THUMBS_ROW_H
    } else {
        DLG_H
    }
}

/// Has the welcome window already been shown on this account?
pub(crate) fn already_shown() -> bool {
    sagethumbs2k_core::settings::get_dword_opt(FIRST_RUN_SHOWN).unwrap_or(0) != 0
}

/// Record that the welcome has been dealt with. Also the `--first-run-seen` entry point:
/// the installer runs that (as the real user, so it reaches the right HKCU) on an UPGRADE,
/// because someone who already had SageThumbs installed has already made these choices and
/// must not be greeted like a new user.
pub(crate) fn mark_shown() {
    let _ = sagethumbs2k_core::settings::set_dword(FIRST_RUN_SHOWN, 1);
}

/// Show the welcome window and block until it is dismissed. No-op if it has run before.
/// Called just before the Settings window opens, which is what the installer launches.
pub(crate) unsafe fn show_if_first_run() {
    if already_shown() {
        return;
    }
    // Mark it BEFORE showing. If anything below panics or the process is killed mid-window,
    // the user gets a working app that simply never asked again — far better than a welcome
    // screen that reappears at every launch.
    mark_shown();
    run_dialog(
        w!("SageThumbs2KFirstRun"),
        Some(first_run_wndproc),
        t("fr_title"),
        DLG_W,
        dlg_h(),
        None,
    );
}

/// The PrtScn choice only means anything while the screenshot hotkey is on — grey it out
/// (and clear it) otherwise, rather than leaving a live control that does nothing.
unsafe fn sync_prtscn(hwnd: HWND) {
    let on = checked(hwnd, ID_SHOT);
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_PRTSCN) {
        let _ = EnableWindow(h, on);
    }
    if !on {
        check(hwnd, ID_PRTSCN, false);
    }
}

/// Build page 2: two more opt-ins, page-1 style. Created lazily when Next is clicked.
unsafe fn build_page2(hwnd: HWND, hinst: HINSTANCE) {
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let unit = dpi_scale(hwnd, 100).max(1);
    let cw = (rc.right - rc.left) * 100 / unit;
    let m = 20;
    let w = cw - m * 2;
    let mut y = 16;

    ctl(
        hwnd,
        STATIC,
        t("fr2_head"),
        WINDOW_STYLE(0),
        m,
        y,
        w,
        20,
        ID_P2_HEAD,
        hinst,
    );
    y += 32;
    for (id, sub_id, key, sub_key) in [
        (ID_P_COVERS, ID_P_COVERS_SUB, "fr2_covers", "fr2_covers_sub"),
        (
            ID_P_SCANLATION,
            ID_P_SCANLATION_SUB,
            "fr2_scanlation",
            "fr2_scanlation_sub",
        ),
    ] {
        ctl(
            hwnd,
            BUTTON,
            t(key),
            WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP,
            m,
            y,
            w,
            20,
            id,
            hinst,
        );
        y += 22;
        ctl(
            hwnd,
            STATIC,
            t(sub_key),
            WINDOW_STYLE(0),
            m + 20,
            y,
            w - 20,
            32,
            sub_id,
            hinst,
        );
        y += 46;
    }
    y += 4;
    ctl(
        hwnd,
        STATIC,
        t("fr2_sub"),
        WINDOW_STYLE(0),
        m,
        y,
        w,
        20,
        ID_P2_SUB,
        hinst,
    );
}

/// Swap page 1 out for page 2 and relabel the button from Next to Get started.
unsafe fn flip_to_page2(hwnd: HWND, hinst: HINSTANCE) {
    for id in [
        ID_HEAD,
        ID_PREVIEW,
        ID_PREVIEW_SUB,
        ID_SHOT,
        ID_SHOT_SUB,
        ID_PRTSCN,
        ID_THUMBS,
        ID_THUMBS_SUB,
    ] {
        if let Ok(c) = GetDlgItem(Some(hwnd), id) {
            let _ = ShowWindow(c, SW_HIDE);
        }
    }
    build_page2(hwnd, hinst);
    if let Ok(b) = GetDlgItem(Some(hwnd), IDOK) {
        let txt = crate::win::wide(t("fr_go"));
        let _ = SetWindowTextW(b, windows::core::PCWSTR(txt.as_ptr()));
    }
    ON_PAGE_2.with(|p| p.set(true));
}

/// Apply the two page-2 switches. Each maps 1:1 to the Settings row that owns it.
unsafe fn apply_persona(hwnd: HWND) {
    use sagethumbs2k_core::settings as s;
    if checked(hwnd, ID_P_COVERS) {
        let _ = s::set_prefer_cover_art(true);
    }
    if checked(hwnd, ID_P_SCANLATION) {
        let _ = s::set_dword("ContainerSkipScanlation", 1);
    }
}

unsafe fn build(hwnd: HWND, hinst: HINSTANCE) {
    // Lay out against the REAL client area in design px (`run_dialog`'s w/h size the whole
    // WINDOW), the same way the feedback dialog does.
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let unit = dpi_scale(hwnd, 100).max(1);
    let cw = (rc.right - rc.left) * 100 / unit;

    let m = 20; // margin
    let w = cw - m * 2;
    let mut y = 16;

    // The stock intro says thumbnails are ALREADY working, which is true of an installed copy
    // and flatly false of a portable one — nothing is registered until the row below is ticked.
    // A portable user who read the installed wording would reasonably conclude the app is broken.
    let portable = offers_thumbnails();
    ctl(
        hwnd,
        STATIC,
        t(if portable {
            "fr_intro_portable"
        } else {
            "fr_intro"
        }),
        WINDOW_STYLE(0),
        m,
        y,
        w,
        34,
        ID_HEAD,
        hinst,
    );
    y += 46;

    if portable {
        ctl(
            hwnd,
            BUTTON,
            t("fr_thumbs"),
            WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP,
            m,
            y,
            w,
            20,
            ID_THUMBS,
            hinst,
        );
        y += 22;
        ctl(
            hwnd,
            STATIC,
            t("fr_thumbs_sub"),
            WINDOW_STYLE(0),
            m + 20,
            y,
            w - 20,
            32,
            ID_THUMBS_SUB,
            hinst,
        );
        y += 46;
    }

    ctl(
        hwnd,
        BUTTON,
        t("fr_preview"),
        WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP,
        m,
        y,
        w,
        20,
        ID_PREVIEW,
        hinst,
    );
    y += 22;
    ctl(
        hwnd,
        STATIC,
        t("fr_preview_sub"),
        WINDOW_STYLE(0),
        m + 20,
        y,
        w - 20,
        32,
        ID_PREVIEW_SUB,
        hinst,
    );
    y += 46;

    ctl(
        hwnd,
        BUTTON,
        t("fr_shot"),
        WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP,
        m,
        y,
        w,
        20,
        ID_SHOT,
        hinst,
    );
    y += 22;
    ctl(
        hwnd,
        STATIC,
        t("fr_shot_sub"),
        WINDOW_STYLE(0),
        m + 20,
        y,
        w - 20,
        18,
        ID_SHOT_SUB,
        hinst,
    );
    y += 24;
    ctl(
        hwnd,
        BUTTON,
        t("fr_prtscn"),
        WINDOW_STYLE(BS_AUTOCHECKBOX as u32 | BS_MULTILINE as u32) | WS_TABSTOP,
        m + 20,
        y,
        w - 20,
        34,
        ID_PRTSCN,
        hinst,
    );

    // Both offers start ticked — this window exists because nobody was finding these
    // features, and the button is an explicit confirmation either way. PrtScn does NOT:
    // it overrides a Windows shortcut, so it stays an opt-in inside an opt-in.
    check(hwnd, ID_PREVIEW, true);
    check(hwnd, ID_SHOT, true);
    // Ticked for the same reason as the other two, and unlike PrtScn it takes nothing away from
    // the user: it adds a handler under their own account and Settings undoes it. Leaving it
    // clear would reproduce the exact failure this row exists to stop — a portable copy whose
    // headline feature silently does nothing.
    if portable {
        check(hwnd, ID_THUMBS, true);
    }
    sync_prtscn(hwnd);

    let bw = 130;
    let bh = 30;
    ctl(
        hwnd,
        BUTTON,
        t("fr_next"),
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
        cw - m - bw,
        (rc.bottom - rc.top) * 100 / unit - bh - 16,
        bw,
        bh,
        IDOK,
        hinst,
    );
}

thread_local! {
    static APPLIED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Apply the ticked choices. Only called from the button — dismissing with the X changes
/// nothing, which is the honest reading of "close without answering". Runs once: the
/// button hits this on BOTH pages (page 1 applies before the flip, so closing the window
/// mid-question still honors confirmed choices), and the daemon start is not free.
unsafe fn apply(hwnd: HWND) {
    if APPLIED.with(|a| a.replace(true)) {
        return;
    }
    // Portable only, and best-effort: a refused registry write must not stop the other choices
    // from being applied. The Settings ▸ Advanced row reports the real state and retries.
    if offers_thumbnails() && checked(hwnd, ID_THUMBS) {
        if let Some(dll) = sagethumbs2k_core::register::dll_beside_exe() {
            if dll.exists() {
                let _ = sagethumbs2k_core::register::register_user(&dll.to_string_lossy());
            }
        }
    }
    if checked(hwnd, ID_PREVIEW) {
        let _ = sagethumbs2k_core::settings::set_preview_enabled(true);
    }
    if checked(hwnd, ID_SHOT) {
        if checked(hwnd, ID_PRTSCN) {
            let _ = sagethumbs2k_core::settings::set_screenshot_hotkey(PRTSCN_ONLY);
            release_windows_prtscn();
        }
        // Last: `set_enabled` reconciles the autostart entry AND starts the daemon, which
        // reads the hotkey settings at startup — so the hotkey has to be persisted first
        // or the fresh daemon would register Ctrl+PrtScn and ignore the choice above.
        crate::screenshot::set_enabled(true);
    } else if checked(hwnd, ID_PREVIEW) {
        // Quick preview alone still needs the resident helper (it owns the Space hook);
        // `heal_if_wanted` is what notices the feature is now wanted and brings it up.
        crate::screenshot::heal_if_wanted();
    }
}

/// Hand the Print Screen key back to applications by clearing Windows' own
/// "PrtScn opens Snipping Tool" binding. Only ever called because the user ticked the box
/// that says this, and only for this user (HKCU). Best-effort.
fn release_windows_prtscn() {
    if let Ok(k) = windows_registry::CURRENT_USER.create(SNIP_KEY_PATH) {
        let _ = k.set_u32(SNIP_KEY_VALUE, 0);
    }
}

extern "system" fn first_run_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        // The intro and the two sub-captions are supporting text, not labels — muted before
        // the generic static coloring claims them.
        if msg == WM_CTLCOLORSTATIC {
            let id = GetDlgCtrlID(HWND(lparam.0 as *mut c_void));
            if id == ID_HEAD
                || id == ID_PREVIEW_SUB
                || id == ID_SHOT_SUB
                || id == ID_THUMBS_SUB
                || id == ID_P_COVERS_SUB
                || id == ID_P_SCANLATION_SUB
                || id == ID_P2_SUB
            {
                return dark_ctlcolor_dim(wparam);
            }
        }
        if let Some(r) = dark_ctlcolor(msg, wparam) {
            return r;
        }
        match msg {
            WM_CREATE => {
                let hinst: HINSTANCE = match GetModuleHandleW(None) {
                    Ok(h) => h.into(),
                    Err(_) => return LRESULT(-1),
                };
                build(hwnd, hinst);
                LRESULT(0)
            }
            WM_COMMAND => {
                match (wparam.0 & 0xFFFF) as i32 {
                    ID_SHOT => sync_prtscn(hwnd),
                    IDOK => {
                        if ON_PAGE_2.with(|p| p.get()) {
                            apply(hwnd);
                            apply_persona(hwnd);
                            let _ = DestroyWindow(hwnd);
                        } else {
                            // Apply page 1 NOW, then ask the one page-2 question. Applying
                            // per-page means closing the window mid-question still honors
                            // the choices already confirmed.
                            apply(hwnd);
                            if let Ok(h) = GetModuleHandleW(None) {
                                flip_to_page2(hwnd, h.into());
                            }
                        }
                    }
                    _ => {}
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
            // Top-level (`run_dialog` with modal: None) pumps until WM_QUIT, so this one
            // MUST post it — unlike the nested-modal dialogs, which must not.
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Headless capture of PAGE 2 (`--window firstrun2`). Flips the freshly built window
/// before capturing; page-1 choices are NOT applied (the flip path that applies them is
/// the button handler, deliberately not exercised here).
pub(crate) unsafe fn run_shot_first_run2(out: &str) -> bool {
    let hinst: HINSTANCE = match GetModuleHandleW(None) {
        Ok(h) => h.into(),
        Err(_) => return false,
    };
    let Some(hwnd) = crate::win::create_shot_window(
        hinst,
        crate::dark::is_dark(),
        w!("SageThumbs2KFirstRunShot2"),
        Some(first_run_wndproc),
        t("fr_title"),
        DLG_W,
        dlg_h(),
    ) else {
        return false;
    };
    flip_to_page2(hwnd, hinst);
    crate::win::pump_msgs(20);
    crate::win::force_repaint(hwnd);
    crate::win::pump_msgs(8);
    crate::win::force_repaint(hwnd);
    let ok = crate::screenshot::capture_hwnd_to_png(hwnd, std::path::Path::new(out));
    let _ = DestroyWindow(hwnd);
    ok
}

/// Headless capture (`--shot <out.png> --window firstrun`) so the layout is verifiable
/// without opening a window or touching any setting.
pub(crate) unsafe fn run_shot_first_run(out: &str) -> bool {
    let hinst: HINSTANCE = match GetModuleHandleW(None) {
        Ok(h) => h.into(),
        Err(_) => return false,
    };
    let Some(hwnd) = crate::win::create_shot_window(
        hinst,
        crate::dark::is_dark(),
        w!("SageThumbs2KFirstRunShot"),
        Some(first_run_wndproc),
        t("fr_title"),
        DLG_W,
        dlg_h(),
    ) else {
        return false;
    };
    crate::win::pump_msgs(20);
    crate::win::force_repaint(hwnd);
    crate::win::pump_msgs(8);
    crate::win::force_repaint(hwnd);
    let ok = crate::screenshot::capture_hwnd_to_png(hwnd, std::path::Path::new(out));
    let _ = DestroyWindow(hwnd);
    ok
}
