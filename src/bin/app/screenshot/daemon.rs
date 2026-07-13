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
use core::sync::atomic::{AtomicU32, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
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
/// The user-assignable "custom action" hotkey (colour picker / convert / rotate / … —
/// see [`crate::hotkey`]). Spawns `--hotkey-action`, which runs whichever action is bound.
const CUSTOM_HOTKEY_ID: i32 = 3;
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
/// Mutual supervision: re-ensure our [`watchdog`](super::watchdog) is alive on a short timer
/// (the watchdog does the same for us). Either process dying alone is then recovered within
/// seconds; single-instance means re-ensuring is a no-op when it's already up.
const WATCHDOG_TIMER_ID: usize = 10;
const WATCHDOG_TIMER_MS: u32 = 5000;
/// Periodic re-assertion of the global hotkey registrations. A `RegisterHotKey` binding can be
/// silently dropped while THIS process keeps running — most notably across sleep/resume, session
/// lock/unlock, and RDP reconnect — after which the hotkey just stops firing even though the tray
/// icon and the watchdog still see a perfectly "healthy" daemon window (the watchdog only checks
/// the window exists, not that the binding is live). The known triggers re-arm instantly (see the
/// `WM_POWERBROADCAST` / `WM_WTSSESSION_CHANGE` / `WM_DISPLAYCHANGE` arms); this slow timer is the
/// catch-all backstop so ANY unforeseen loss self-heals within a minute instead of staying dead
/// until the user reopens the app. Unregister+Register is cheap and idempotent (the same dance
/// `WM_RELOAD` already does), so re-running it when nothing was lost is harmless.
const REARM_TIMER_ID: usize = 11;
const REARM_TIMER_MS: u32 = 60_000;
/// Retry cadence for a tray-icon add the shell rejected. `NIM_ADD` fails when the taskbar
/// isn't up yet — the autostart daemon races Explorer at logon — and a single silent attempt
/// left the icon permanently missing while the daemon ran fine underneath (the user then
/// reads "no icon" as "not running"). Bounded churn: the timer dies on the first success.
const TRAY_RETRY_TIMER_ID: usize = 12;
const TRAY_RETRY_MS: u32 = 3000;

/// The shell's dynamic "TaskbarCreated" broadcast id (`RegisterWindowMessageW` — it has no
/// fixed value, so it's resolved at startup and stashed here for the wndproc's match guard).
/// Explorer broadcasts it whenever the taskbar is (re)created: every crash/restart of
/// Explorer destroys ALL notify icons, and any tray app that doesn't re-add on this message
/// loses its icon until the process restarts.
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
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
    // Single instance, TOCTOU-safe: claim a named mutex FIRST. The FindWindow check alone
    // races — autostart, the watchdog's tick, and a Settings-open heal can all spawn a
    // daemon in the same instant, each passing the window check before any has created its
    // window; both then register hotkeys and one silently loses. The OS arbitrates the
    // mutex, so exactly one proceeds. Held (leaked) for process life on purpose.
    let Ok(_lock) = CreateMutexW(None, true, w!("SageThumbs2K.ShotDaemon.Single")) else {
        return;
    };
    if GetLastError() == ERROR_ALREADY_EXISTS {
        return;
    }
    // Belt-and-suspenders (and the check callers use): a daemon window already up = done.
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

    // Quick preview: install the WH_KEYBOARD_LL "press Space to preview" hook if the feature
    // is enabled (no-op otherwise). Re-armed on the same triggers as the hotkeys (below).
    super::spacehook::rearm(hwnd);

    // Learn the shell's dynamic "TaskbarCreated" broadcast id BEFORE the tray add, so an
    // Explorer (re)start from here on always re-adds our icon (see the wndproc arm).
    TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);

    // Tray icon is shown unless the user hid it in Settings (the hotkey still works).
    // `ensure_tray_icon` retries on a timer if the taskbar isn't accepting adds yet.
    if !sagethumbs2k_core::settings::screenshot_hide_tray() {
        ensure_tray_icon(hwnd);
    }

    // Opt-in periodic update check. ONLY this already-resident process does it — there is
    // no scheduled task or service. The actual network hit is throttled to once/day inside
    // `update::lazy_check_worker`; this 6h timer just re-attempts (covering machines left
    // on for days). One check fires shortly after startup, then on each timer tick.
    if sagethumbs2k_core::settings::update_auto_check() {
        let _ = SetTimer(Some(hwnd), UPDATE_TIMER_ID, UPDATE_TIMER_MS, None);
        kick_update_check(hwnd);
    }

    // Bring up our lightweight watchdog so this daemon gets restarted if it ever dies
    // (a `panic = "abort"` build takes the whole process — and all hotkeys — down on any
    // panic). Only while we're actually wanted at logon; single-instance, so it's a no-op
    // if one's already supervising. This also protects existing installs whose autostart
    // still launches the daemon directly, with no autostart-entry migration. The timer then
    // re-ensures it (mutual supervision) so a lone watchdog death is also recovered.
    if super::supervise_wanted() {
        super::ensure_watchdog();
    }
    let _ = SetTimer(Some(hwnd), WATCHDOG_TIMER_ID, WATCHDOG_TIMER_MS, None);

    // Keep the hotkeys alive across events that silently drop `RegisterHotKey` bindings while
    // this process stays up. Session notifications (lock/unlock, connect/disconnect, RDP
    // reconnect) need an explicit opt-in to reach our window; power-resume + display-change
    // broadcasts arrive automatically. The periodic re-arm timer is the catch-all backstop.
    let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
    let _ = SetTimer(Some(hwnd), REARM_TIMER_ID, REARM_TIMER_MS, None);

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
    // Registration stays best-effort (a taken chord must not stop the tray/daemon), but
    // the failures are no longer INVISIBLE: a bitmask of which bindings failed (bit0
    // capture, bit1 quick-save, bit2 custom action) is persisted so the Settings status
    // line can say "hotkey in use by another app" instead of a flat "Running". Every
    // re-arm rewrites it, so releasing the conflicting app self-clears within a minute.
    let mut failed = 0u32;
    // The capture + quick-save hotkeys belong to the SCREENSHOT feature — register them only
    // while it's enabled, so a daemon kept alive solely for a custom hotkey doesn't also grab
    // Ctrl+PrtScn.
    if super::is_enabled() {
        let (hkf, vk) = sagethumbs2k_core::settings::screenshot_hotkey();
        if RegisterHotKey(Some(hwnd), HOTKEY_ID, hkf_to_mods(hkf), vk).is_err() {
            failed |= 1;
        }
        let (qhkf, qvk) = sagethumbs2k_core::settings::screenshot_quick_hotkey();
        if qvk != 0 && RegisterHotKey(Some(hwnd), QUICK_HOTKEY_ID, hkf_to_mods(qhkf), qvk).is_err()
        {
            failed |= 2;
        }
    }
    // The user-assignable custom action hotkey — independent of the screenshot feature.
    let (chkf, cvk) = sagethumbs2k_core::settings::custom_action_hotkey();
    if cvk != 0 && RegisterHotKey(Some(hwnd), CUSTOM_HOTKEY_ID, hkf_to_mods(chkf), cvk).is_err() {
        failed |= 4;
    }
    let _ = sagethumbs2k_core::settings::set_dword("HotkeyBindFailed", failed);
}

/// Drop and re-create every global hotkey registration from the current settings. Called by the
/// periodic backstop timer, the power/session/display re-arm triggers, and [`WM_RELOAD`] after a
/// settings change. Unregister-then-register is idempotent: if a binding is still live this is a
/// harmless no-op churn; if it was silently lost (sleep/resume, unlock, RDP reconnect …) this is
/// what brings it back — without the user having to reopen the app.
unsafe fn rearm_hotkeys(hwnd: HWND) {
    let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
    let _ = UnregisterHotKey(Some(hwnd), QUICK_HOTKEY_ID);
    let _ = UnregisterHotKey(Some(hwnd), CUSTOM_HOTKEY_ID);
    register_configured_hotkey(hwnd);
    // The Quick preview Space hook rides the SAME recovery discipline: reinstall it (or remove
    // it if the feature was just turned off in Settings via WM_RELOAD). Windows can silently
    // drop a slow LL hook across sleep/resume/session-change, exactly like a RegisterHotKey.
    super::spacehook::rearm(hwnd);
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

/// Add the tray icon, retrying on a short timer until the shell accepts it. `NIM_ADD`
/// fails when the taskbar doesn't exist yet (autostart at logon racing Explorer) — one
/// silent attempt left the icon permanently missing. If the add fails because the icon
/// is ALREADY there (a redundant call), `NIM_MODIFY` succeeds and settles it — so the
/// retry timer only survives genuine "no taskbar yet" failures.
unsafe fn ensure_tray_icon(hwnd: HWND) {
    let nid = tray_data(hwnd, true);
    if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() || Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool()
    {
        let _ = KillTimer(Some(hwnd), TRAY_RETRY_TIMER_ID);
    } else {
        let _ = SetTimer(Some(hwnd), TRAY_RETRY_TIMER_ID, TRAY_RETRY_MS, None);
    }
}

unsafe fn remove_tray_icon(hwnd: HWND) {
    // Cancel any pending add-retry too, so a hide can't be undone by a late retry tick.
    let _ = KillTimer(Some(hwnd), TRAY_RETRY_TIMER_ID);
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
                    CUSTOM_HOTKEY_ID => spawn(Some("--hotkey-action")),
                    _ => {}
                }
                LRESULT(0)
            }
            // Quick preview Space hook (see `spacehook`): the hook callback posts these so the
            // heavier FindWindow / spawn / WM_COPYDATA work happens OFF the LL-hook callback.
            m if m == super::spacehook::WM_APP_PREVIEW => {
                crate::preview::request_toggle();
                LRESULT(0)
            }
            m if m == super::spacehook::WM_APP_PREVIEW_CLOSE => {
                crate::preview::request_close();
                LRESULT(0)
            }
            WM_RELOAD => {
                rearm_hotkeys(hwnd);
                // Reconcile the tray icon with the (possibly just-changed) setting.
                if sagethumbs2k_core::settings::screenshot_hide_tray() {
                    remove_tray_icon(hwnd);
                } else {
                    ensure_tray_icon(hwnd);
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
                match wparam.0 {
                    UPDATE_TIMER_ID => kick_update_check(hwnd),
                    // Re-check the watchdog is alive; re-spawn it if it isn't and we're still
                    // wanted (a manually-launched daemon with no autostart entry won't — the
                    // guard then falls through to the no-op arm below).
                    WATCHDOG_TIMER_ID if super::supervise_wanted() => super::ensure_watchdog(),
                    // Catch-all backstop: re-assert the hotkey registrations in case some
                    // unforeseen event silently dropped them while we kept running.
                    REARM_TIMER_ID => rearm_hotkeys(hwnd),
                    // The taskbar rejected our icon earlier (logon race) — try again until
                    // it takes, unless the user hid the icon meanwhile.
                    TRAY_RETRY_TIMER_ID => {
                        if sagethumbs2k_core::settings::screenshot_hide_tray() {
                            let _ = KillTimer(Some(hwnd), TRAY_RETRY_TIMER_ID);
                        } else {
                            ensure_tray_icon(hwnd);
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            // Sleep/resume, lock/unlock, RDP reconnect and display changes can each silently drop
            // a live `RegisterHotKey` while this process stays up — so the hotkey quietly dies and
            // the watchdog (which only sees the window) never notices. Re-assert on each event so
            // the hotkey comes back the instant the machine does, with no "reopen the app" needed.
            WM_POWERBROADCAST => {
                // Only on RESUME — never on suspend, so we never release the chord right before
                // sleeping (which would leave it unregistered until wake).
                let ev = wparam.0 as u32;
                if ev == PBT_APMRESUMEAUTOMATIC || ev == PBT_APMRESUMESUSPEND {
                    rearm_hotkeys(hwnd);
                }
                LRESULT(1) // TRUE — grant the power-state change
            }
            WM_WTSSESSION_CHANGE => {
                // Any session transition (lock/unlock, connect/disconnect) is cheap to re-arm on.
                rearm_hotkeys(hwnd);
                LRESULT(0)
            }
            WM_DISPLAYCHANGE => {
                rearm_hotkeys(hwnd);
                LRESULT(0)
            }
            // Explorer was (re)started: the fresh taskbar has NO notify icons — every tray
            // app must re-add its own on this broadcast or its icon is gone for good while
            // the process runs on invisibly. Also covers the logon race (daemon up before
            // the taskbar). Explorer restarts are ROUTINE around this app: its own installer
            // and dev install script restart Explorer to swap the shell-extension DLL.
            m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
                if !sagethumbs2k_core::settings::screenshot_hide_tray() {
                    ensure_tray_icon(hwnd);
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
                        // "Exit" disables the daemon for real: drop the HKCU autostart entry
                        // (so it won't relaunch at next logon) AND close the daemon (quit posts
                        // WM_CLOSE → WM_DESTROY, which removes the tray icon + unregisters the
                        // hotkeys). Unlike `set_enabled(false)`, `quit` stops even when a custom
                        // hotkey is bound — an explicit "stop everything".
                        super::quit();
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = KillTimer(Some(hwnd), UPDATE_TIMER_ID);
                let _ = KillTimer(Some(hwnd), WATCHDOG_TIMER_ID);
                let _ = KillTimer(Some(hwnd), REARM_TIMER_ID);
                let _ = WTSUnRegisterSessionNotification(hwnd);
                super::spacehook::uninstall(); // drop the Space hook with the daemon
                remove_tray_icon(hwnd);
                let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
                let _ = UnregisterHotKey(Some(hwnd), QUICK_HOTKEY_ID);
                let _ = UnregisterHotKey(Some(hwnd), CUSTOM_HOTKEY_ID);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
