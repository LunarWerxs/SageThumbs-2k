//! The opt-in background helper for screenshot hotkeys (`--screenshot-daemon`).
//!
//! A tiny **per-user tray app** — NOT a Windows service: it only runs when the
//! user enables screenshots in Settings (which writes an HKCU autostart entry and
//! launches this), and stops when they disable it. Default state = nothing
//! running, so the "no background bloat" promise holds. It registers a global
//! hotkey (default Ctrl+PrtScn) and, on press, spawns the capture overlay
//! (`--screenshot`) as a SEPARATE process so a capture can't take the tray down.
//! A tray icon offers Capture / Settings / Quit. Single-instance (FindWindow).

use core::mem::size_of;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, ShellExecuteW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::win::{app_icon, wide};

const HOTKEY_ID: i32 = 1;
/// The optional second "quick-save" hotkey (full-screen → clipboard+PNG, no editor).
const QUICK_HOTKEY_ID: i32 = 2;
const WM_TRAY: u32 = WM_APP + 1;
/// Posted by the Settings window (via `enable::reload_hotkey`) when the user picks
/// a different capture hotkey, so a running daemon re-reads + re-registers it.
pub(super) const WM_RELOAD: u32 = WM_APP + 2;
const TRAY_UID: u32 = 1;
const IDM_CAPTURE: usize = 101;
const IDM_SETTINGS: usize = 102;
const IDM_QUIT: usize = 103;
const IDM_HIDE: usize = 104;
/// Periodic update check (only this already-resident process runs it — no scheduled task).
const UPDATE_TIMER_ID: usize = 9;
/// Re-attempt every 6h; `update::lazy_check_worker` throttles the actual network hit to 1/day.
const UPDATE_TIMER_MS: u32 = 6 * 60 * 60 * 1000;
/// A newer release was found (lparam = `Box<String>` tag); posted from the check thread.
const WM_UPDATE_FOUND: u32 = WM_APP + 3;
/// The user clicked our update toast (Shell tray notification balloon).
const NIN_BALLOONUSERCLICK: u32 = 0x0405;

pub(super) const CLASS: PCWSTR = w!("SageThumbs2KShotDaemon");

/// Spawn a fresh instance of ourselves in the requested mode (capture overlay, or
/// the Settings window). A separate process keeps the tray alive across captures.
fn spawn(arg: Option<&str>) {
    match arg {
        Some(a) => super::spawn_self(&[a]),
        None => super::spawn_self(&[]),
    }
}

pub(crate) unsafe fn run_daemon(hinst: HINSTANCE) {
    // Single instance: if a daemon window already exists, don't start a second.
    if FindWindowW(CLASS, PCWSTR::null()).is_ok() {
        return;
    }

    let wc = WNDCLASSW {
        lpfnWndProc: Some(daemon_wndproc),
        hInstance: hinst,
        lpszClassName: CLASS,
        ..Default::default()
    };
    RegisterClassW(&wc);

    // A normal but never-shown window (hosts the tray icon + receives WM_HOTKEY).
    let Ok(hwnd) = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        CLASS,
        w!("SageThumbs 2K Screenshot Daemon"),
        WS_OVERLAPPED,
        0,
        0,
        0,
        0,
        None,
        None,
        Some(hinst),
        None,
    ) else {
        return;
    };

    // Global hotkey (user-configurable in Settings; default Ctrl+PrtScn — PrtScn
    // alone is claimed by Win11's Snipping Tool). Best-effort — if it's taken, the
    // tray menu still works.
    register_configured_hotkey(hwnd);

    // Tray icon is shown unless the user hid it in Settings (the hotkey still works).
    if !sagethumbs2k_core::settings::screenshot_hide_tray() {
        add_tray_icon(hwnd);
    }

    // Opt-in periodic update check. ONLY this already-resident process does it — there is
    // no scheduled task or service. The actual network hit is throttled to once/day inside
    // `update::lazy_check_worker`; this 6h timer just re-attempts (covering machines left
    // on for days). One check fires shortly after startup, then on each timer tick.
    if sagethumbs2k_core::settings::update_auto_check() {
        let _ = SetTimer(Some(hwnd), UPDATE_TIMER_ID, UPDATE_TIMER_MS, None);
        kick_update_check(hwnd);
    }

    let mut msg = MSG::default();
    loop {
        let r = GetMessageW(&mut msg, None, 0, 0).0;
        if r == 0 || r == -1 {
            break;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

/// (Re-)register the global capture hotkey from the persisted setting, converting
/// the stored HOTKEYF_* modifiers to RegisterHotKey's MOD_* flags. Best-effort.
/// Convert stored HOTKEYF_* modifier bits (SHIFT 0x01, CONTROL 0x02, ALT 0x04) to
/// RegisterHotKey's MOD_* flags, always with MOD_NOREPEAT so a held chord fires once.
fn hkf_to_mods(hkf: u32) -> HOT_KEY_MODIFIERS {
    let mut mods = MOD_NOREPEAT;
    if hkf & 0x01 != 0 {
        mods |= MOD_SHIFT;
    }
    if hkf & 0x02 != 0 {
        mods |= MOD_CONTROL;
    }
    if hkf & 0x04 != 0 {
        mods |= MOD_ALT;
    }
    mods
}

/// (Re-)register BOTH the main capture hotkey and the optional quick-save hotkey
/// from the persisted settings. Best-effort — if a chord is taken the tray menu
/// still works. The quick hotkey is skipped when its vk is 0 (disabled).
unsafe fn register_configured_hotkey(hwnd: HWND) {
    let (hkf, vk) = sagethumbs2k_core::settings::screenshot_hotkey();
    let _ = RegisterHotKey(Some(hwnd), HOTKEY_ID, hkf_to_mods(hkf), vk);
    let (qhkf, qvk) = sagethumbs2k_core::settings::screenshot_quick_hotkey();
    if qvk != 0 {
        let _ = RegisterHotKey(Some(hwnd), QUICK_HOTKEY_ID, hkf_to_mods(qhkf), qvk);
    }
}

/// Build a NOTIFYICONDATAW for our tray entry (hWnd + uID identify it for ADD/DELETE).
unsafe fn tray_data(hwnd: HWND, with_payload: bool) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        ..Default::default()
    };
    if with_payload {
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = app_icon().unwrap_or_default();
        let tip = wide(&format!("SageThumbs 2K — Screenshot ({})", hotkey_label()));
        for (d, s) in nid.szTip.iter_mut().zip(tip.iter()) {
            *d = *s;
        }
    }
    nid
}

/// Human label for the currently-configured capture hotkey (e.g. "Ctrl + PrtScn"),
/// for the tray tooltip — so a remapped hotkey is shown correctly instead of the
/// hardcoded default. The stored value always comes from the Settings dropdown, so
/// it matches one of the presets; an unknown value falls back to the default label.
fn hotkey_label() -> &'static str {
    let (m, v) = sagethumbs2k_core::settings::screenshot_hotkey();
    let packed = (m << 8) | v;
    crate::settings_dlg::SHOT_PRESETS
        .iter()
        .find(|&&(_, p)| p == packed)
        .map_or("Ctrl + PrtScn", |&(label, _)| label)
}

unsafe fn add_tray_icon(hwnd: HWND) {
    let nid = tray_data(hwnd, true);
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
}

unsafe fn remove_tray_icon(hwnd: HWND) {
    let nid = tray_data(hwnd, false);
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

unsafe fn show_tray_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let _ = AppendMenuW(menu, MF_STRING, IDM_CAPTURE, w!("Take Screenshot"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_SETTINGS, w!("Settings"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_HIDE, w!("Hide tray icon"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, IDM_QUIT, w!("Quit"));
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // Required so the menu dismisses when the user clicks elsewhere.
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, None, hwnd, None);
    let _ = DestroyMenu(menu);
}

/// Spawn the throttled, Worker-routed update check on a background thread. If it finds a
/// newer release it posts `WM_UPDATE_FOUND` back to the daemon window (which owns the tray
/// icon) carrying the version tag in a `Box<String>`.
unsafe fn kick_update_check(hwnd: HWND) {
    let hwnd_raw = hwnd.0 as isize; // HWND isn't Send; ferry the raw handle to the worker.
    crate::update::lazy_check_worker(move |tag| unsafe {
        let boxed = Box::into_raw(Box::new(tag)) as isize;
        // PostMessageW is safe cross-thread; the UI thread reclaims the box.
        if PostMessageW(
            Some(HWND(hwnd_raw as *mut core::ffi::c_void)),
            WM_UPDATE_FOUND,
            WPARAM(0),
            LPARAM(boxed),
        )
        .is_err()
        {
            drop(Box::from_raw(boxed as *mut String)); // window gone — don't leak
        }
    });
}

/// Pop a tray "update available" balloon (clickable → the releases page). A no-op if the
/// tray icon is hidden, in which case the next Settings open still surfaces the update.
unsafe fn show_update_toast(hwnd: HWND, tag: &str) {
    let mut nid = tray_data(hwnd, false);
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO;
    let title = wide("SageThumbs 2K update available");
    let info = wide(&format!("Version {tag} is ready — click to download."));
    for (d, s) in nid.szInfoTitle.iter_mut().zip(title.iter()) {
        *d = *s;
    }
    for (d, s) in nid.szInfo.iter_mut().zip(info.iter()) {
        *d = *s;
    }
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

/// Open the GitHub releases page in the default browser (the update toast's click target).
unsafe fn open_releases() {
    let url = wide(crate::update::RELEASES_URL);
    ShellExecuteW(
        None,
        w!("open"),
        PCWSTR(url.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}

extern "system" fn daemon_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_HOTKEY => {
                match wparam.0 as i32 {
                    HOTKEY_ID => spawn(Some("--screenshot")),
                    QUICK_HOTKEY_ID => spawn(Some("--screenshot-instant")),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_RELOAD => {
                let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
                let _ = UnregisterHotKey(Some(hwnd), QUICK_HOTKEY_ID);
                register_configured_hotkey(hwnd);
                // Reconcile the tray icon with the (possibly just-changed) setting.
                if sagethumbs2k_core::settings::screenshot_hide_tray() {
                    remove_tray_icon(hwnd);
                } else {
                    add_tray_icon(hwnd);
                }
                LRESULT(0)
            }
            WM_TRAY => {
                let ev = (lparam.0 & 0xffff) as u32;
                if ev == WM_LBUTTONDBLCLK {
                    spawn(Some("--screenshot"));
                } else if ev == WM_RBUTTONUP || ev == WM_CONTEXTMENU {
                    show_tray_menu(hwnd);
                } else if ev == NIN_BALLOONUSERCLICK {
                    open_releases(); // clicked the "update available" toast
                }
                LRESULT(0)
            }
            WM_TIMER => {
                if wparam.0 == UPDATE_TIMER_ID {
                    kick_update_check(hwnd);
                }
                LRESULT(0)
            }
            WM_UPDATE_FOUND => {
                if lparam.0 != 0 {
                    let tag = *Box::from_raw(lparam.0 as *mut String);
                    show_update_toast(hwnd, &tag);
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                match wparam.0 & 0xffff {
                    IDM_CAPTURE => spawn(Some("--screenshot")),
                    IDM_SETTINGS => spawn(None),
                    IDM_HIDE => {
                        // Hide the tray icon but keep the hotkey running (matches the
                        // Settings "Hide tray icon" toggle). Restore via Settings.
                        let _ = sagethumbs2k_core::settings::set_dword("ScreenshotHideTray", 1);
                        remove_tray_icon(hwnd);
                    }
                    IDM_QUIT => {
                        // "Exit" disables the hotkey for real: drop the HKCU autostart
                        // entry (so it won't relaunch at next logon) AND close the
                        // daemon (set_enabled(false) posts WM_CLOSE → WM_DESTROY, which
                        // removes the tray icon + unregisters the hotkey).
                        super::set_enabled(false);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = KillTimer(Some(hwnd), UPDATE_TIMER_ID);
                remove_tray_icon(hwnd);
                let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
                let _ = UnregisterHotKey(Some(hwnd), QUICK_HOTKEY_ID);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
