//! The opt-in background helper for screenshot hotkeys (`--screenshot-daemon`).
//!
//! A tiny **per-user tray app** — NOT a Windows service: it only runs when the
//! user enables screenshots in Settings (which writes an HKCU autostart entry and
//! launches this), and stops when they disable it. Default state = nothing
//! running, so the "no background bloat" promise holds. It registers a global
//! hotkey (default Ctrl+PrtScn) and, on press, spawns the capture overlay
//! (`--screenshot`) as a SEPARATE process so a capture can't take the tray down.
//! A tray icon offers Capture / Settings / Quit. Single-instance (FindWindow).

use core::ffi::c_void;
use core::mem::size_of;
use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Mutex};
mod tray;
use tray::*;
mod hotkeys;
use hotkeys::*;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT,
};
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO,
    NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
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
/// "Copy text on screen (OCR)" — the same capture-overlay OCR mode the custom hotkey can be
/// bound to, reachable with a click so it needs no hotkey set up first.
const IDM_OCR: usize = 105;
/// Periodic update check (only this already-resident process runs it — no scheduled task).
const UPDATE_TIMER_ID: usize = 9;
/// Re-attempt every 6h; `update::lazy_check_worker` throttles the actual network hit to 1/day.
/// That same worker thread is also where the licence entitlement re-check piggybacks (see
/// the call to `license::refresh_entitlement` inside `lazy_check_worker`) — this timer is
/// the only cadence either one needs.
const UPDATE_TIMER_MS: u32 = 6 * 60 * 60 * 1000;
/// Periodic re-assertion of the global hotkey registrations. A `RegisterHotKey` binding can be
/// silently dropped while THIS process keeps running — most notably across sleep/resume, session
/// lock/unlock, and RDP reconnect — after which the hotkey just stops firing even though the tray
/// icon remains present. The known triggers re-arm instantly (see the `WM_POWERBROADCAST` /
/// `WM_WTSSESSION_CHANGE` / `WM_DISPLAYCHANGE` arms); this slow timer is the catch-all backstop
/// so ANY unforeseen loss self-heals within a minute instead of staying dead until the user
/// reopens the app. Unregister+Register is cheap and idempotent (the same dance `WM_RELOAD`
/// already does), so re-running it when nothing was lost is harmless.
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
/// The user clicked one of our tray balloons.
const NIN_BALLOONUSERCLICK: u32 = 0x0405;
/// Which balloon is currently on screen, so a click does the right thing. The tray gives us one
/// undifferentiated "the user clicked the balloon" message, so this is the only way to tell an
/// update toast from the elevated-window warning.
static LAST_BALLOON: AtomicU32 = AtomicU32::new(BALLOON_NONE);
const BALLOON_NONE: u32 = 0;
const BALLOON_UPDATE: u32 = 1;
const BALLOON_ELEVATED: u32 = 2;
/// The business-licence reminder (evaluation countdown, notice, or stopped); a click opens
/// Settings on the Licence page.
const BALLOON_LICENCE: u32 = 3;
/// A licence reminder is due: posted from `kick_licence_tick`'s worker. Carries nothing;
/// the daemon thread re-reads the breadcrumb, same forgery reasoning as `WM_UPDATE_FOUND`.
const WM_LICENCE_DUE: u32 = WM_APP + 4;
/// The licence tick's own 6-hour timer, separate from the update timer because that one is
/// only armed when update checks are on and a licence is not an update.
const LICENCE_TIMER_ID: usize = 13;

pub(super) const CLASS: PCWSTR = w!("SageThumbs2KShotDaemon");

/// Work the wndproc must NOT do itself: the thread that runs `daemon_wndproc` is
/// the same thread that owns the `WH_KEYBOARD_LL` Quick-preview hook (`spacehook::rearm` is
/// called from here, in `run_daemon`), and Windows delays delivery of every keystroke on the
/// machine until that thread answers its next `GetMessage`/`PeekMessage` — then, past
/// `LowLevelHooksTimeout`, silently drops the hook. `CreateProcess` (spawning a capture/preview/
/// hotkey-action helper) and an inter-process `SendMessageW` (`preview::send_command`) both
/// block for long enough to matter, so neither may run inline in the wndproc; a dedicated
/// worker thread does the actual work instead.
enum DaemonWork {
    PreviewToggle,
    PreviewClose,
    Hotkey(i32),
}

/// The sender half of the worker's channel, set once by [`start_worker`] from [`run_daemon`]
/// right after the window is created — before any message that could dispatch to it can
/// arrive. `mpsc::Sender<T>` is `Send` but not `Sync`, so it can't sit in a bare `static`
/// (which requires `Sync`) — the `Mutex` is here purely to satisfy that, not for contention:
/// this daemon has one wndproc thread doing the (brief, non-blocking) sends.
static WORK_TX: Mutex<Option<mpsc::Sender<DaemonWork>>> = Mutex::new(None);

/// Spawn the worker thread and stash the sender the wndproc posts to.
fn start_worker() {
    let (tx, rx) = mpsc::channel::<DaemonWork>();
    std::thread::spawn(move || {
        for work in rx {
            unsafe {
                match work {
                    DaemonWork::PreviewToggle => crate::preview::request_toggle(),
                    DaemonWork::PreviewClose => crate::preview::request_close(),
                    DaemonWork::Hotkey(id) => on_hotkey(id),
                }
            }
        }
    });
    if let Ok(mut slot) = WORK_TX.lock() {
        *slot = Some(tx);
    }
}

/// Hand `work` to the worker thread. A no-op if it somehow isn't running yet (before
/// [`run_daemon`] has called [`start_worker`]) — the hook/wndproc thread must never fall back
/// to running this inline, which is the exact thing this whole indirection exists to avoid.
fn dispatch_to_worker(work: DaemonWork) {
    if let Ok(slot) = WORK_TX.lock() {
        if let Some(tx) = slot.as_ref() {
            let _ = tx.send(work);
        }
    }
}

/// Where `kick_update_check` stashes the found version tag for `on_update_found` to read.
/// `WM_UPDATE_FOUND` is a plain `WM_APP + 3` id, not a `RegisterWindowMessageW`
/// one, and this window's class name (`CLASS`) is a fixed, `FindWindowW`-discoverable
/// constant — so without this, any same-desktop process could post the message itself with an
/// arbitrary `lparam` and make us reconstruct-and-free an attacker-chosen `Box<String>`.
/// Keeping the tag process-local and posting only a token means a forged message just finds
/// nothing to show.
static UPDATE_TAG: Mutex<Option<String>> = Mutex::new(None);

/// Spawn a fresh instance of ourselves in the requested mode (capture overlay, or
/// the Settings window). A separate process keeps the tray alive across captures.
fn spawn(arg: Option<&str>) {
    // Nothing to clean up if it doesn't start (no temp file changes hands here), so the
    // success flag is deliberately dropped.
    let _ = match arg {
        Some(a) => super::spawn_self(&[a]),
        None => super::spawn_self(&[]),
    };
}

pub(crate) unsafe fn run_daemon(hinst: HINSTANCE) {
    // Single instance, TOCTOU-safe: claim a named mutex FIRST. The FindWindow check alone
    // races — autostart and a Settings-open heal can both spawn a daemon in the same
    // instant, each passing the window check before either creates its window; both
    // then register hotkeys and one silently loses. The OS arbitrates the mutex, so
    // exactly one proceeds. Held (leaked) for process life on purpose.
    let (lock, last_err) =
        crate::win::create_mutex_user_only(true, w!("SageThumbs2K.ShotDaemon.Single"));
    let Ok(_lock) = lock else {
        return;
    };
    if last_err == ERROR_ALREADY_EXISTS {
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

    // The worker thread that runs CreateProcess / SendMessageW off the hook thread —
    // must be up before any hotkey/hook message can be dispatched to it.
    start_worker();

    // Global hotkey (user-configurable in Settings; default Ctrl+PrtScn — PrtScn
    // alone is claimed by Win11's Snipping Tool). Best-effort — if it's taken, the
    // tray menu still works.
    register_configured_hotkey(hwnd);

    // Quick preview: install the WH_KEYBOARD_LL "press Space to preview" hook if the feature
    // is enabled (no-op otherwise). Re-armed on the same triggers as the hotkeys (below).
    super::spacehook::rearm(hwnd);
    // The foreground watcher that explains an elevated window (see `elevwarn`) installs HERE
    // too, not only from `rearm_hotkeys`: that runs off the 60 s backstop timer, so relying on
    // it alone left the first minute after logon silently unwatched — which is exactly when a
    // user who just launched an elevated Everything is trying the feature.
    super::elevwarn::rearm(hwnd);

    // Learn the shell's dynamic "TaskbarCreated" broadcast id BEFORE the tray add, so an
    // Explorer (re)start from here on always re-adds our icon (see the wndproc arm).
    TASKBAR_CREATED.store(
        RegisterWindowMessageW(w!("TaskbarCreated")),
        Ordering::Relaxed,
    );

    // Tray icon is shown unless the user hid it in Settings (the hotkey still works).
    // `ensure_tray_icon` retries on a timer if the taskbar isn't accepting adds yet.
    if !sagethumbs2k_core::settings::screenshot_hide_tray() {
        ensure_tray_icon(hwnd);
    }

    // Periodic update check from the already-resident process. NOT the only one: because
    // this helper is opt-in, the check also runs from the per-user `--update-check`
    // Scheduled Task and from any ordinary app launch (see `update::spawn_due_check`). All
    // three share the once/day throttle in `update::check_throttled`, so having the helper
    // running just means the check happens here first. This 6h timer re-attempts (covering
    // machines left on for days); one check fires shortly after startup.
    if sagethumbs2k_core::settings::update_auto_check() {
        let _ = SetTimer(Some(hwnd), UPDATE_TIMER_ID, UPDATE_TIMER_MS, None);
        kick_update_check(hwnd);
    }
    // The business-licence tick: starts the evaluation clock on first sight of a Business
    // copy, refreshes the entitlement, and raises the reminder balloon when one is due.
    // Unconditional (a Personal copy pays one HKLM read per tick) and on its own timer.
    let _ = SetTimer(Some(hwnd), LICENCE_TIMER_ID, UPDATE_TIMER_MS, None);
    kick_licence_tick(hwnd);

    // Keep the hotkeys alive across events that silently drop `RegisterHotKey` bindings while
    // this process stays up. Session notifications (lock/unlock, connect/disconnect, RDP
    // reconnect) need an explicit opt-in to reach our window; power-resume + display-change
    // broadcasts arrive automatically. The periodic re-arm timer is the catch-all backstop.
    let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
    let _ = SetTimer(Some(hwnd), REARM_TIMER_ID, REARM_TIMER_MS, None);

    crate::win::pump_plain();
}

/// Spawn the throttled, Worker-routed update check on a background thread. If it finds a
/// newer release it stashes the tag in [`UPDATE_TAG`] and posts `WM_UPDATE_FOUND` back to the
/// daemon window (which owns the tray icon) carrying no payload at all — `lparam`
/// used to carry a raw `Box<String>` pointer, which any same-desktop process could forge by
/// posting the message itself.
unsafe fn kick_update_check(hwnd: HWND) {
    let hwnd_raw = hwnd.0 as isize; // HWND isn't Send; ferry the raw handle to the worker.
    crate::update::lazy_check_worker(move |tag| {
        if let Ok(mut slot) = UPDATE_TAG.lock() {
            *slot = Some(tag);
        }
        // PostMessageW is safe cross-thread.
        unsafe {
            let _ = PostMessageW(
                Some(HWND(hwnd_raw as *mut core::ffi::c_void)),
                WM_UPDATE_FOUND,
                WPARAM(0),
                LPARAM(0),
            );
        }
    });
}

/// Run the licence tick (`license::background_tick`: start the evaluation if due, refresh
/// the entitlement, decide whether a reminder is due) off the hook thread, and post
/// `WM_LICENCE_DUE` back when one is. Same ferry-the-raw-handle shape as
/// `kick_update_check`.
unsafe fn kick_licence_tick(hwnd: HWND) {
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        if crate::license::background_tick().is_some() {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd_raw as *mut core::ffi::c_void)),
                    WM_LICENCE_DUE,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    });
}

/// `WM_LICENCE_DUE`: re-read the breadcrumb on this thread and pop the reminder balloon
/// (a click opens Settings on the Licence page). Records the nag so the Settings window
/// and the daily one-shot, which share the cadence, do not repeat it the same day. A
/// no-op if the tray icon is hidden, in which case the next Settings open still says it.
unsafe fn on_licence_due(hwnd: HWND) {
    let snap = crate::license::snapshot();
    if !snap.posture.wants_reminder() {
        return;
    }
    LAST_BALLOON.store(BALLOON_LICENCE, Ordering::Relaxed);
    let mut nid = tray_data(hwnd, false);
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = if snap.posture.is_urgent() {
        NIIF_WARNING
    } else {
        NIIF_INFO
    };
    set_balloon_text(&mut nid.szInfoTitle, crate::win::t("licence_popup_title"));
    set_balloon_text(
        &mut nid.szInfo,
        &format!(
            "{} {}",
            crate::settings_dlg::licence_reminder_body(&snap),
            crate::win::t("licence_toast_enter_key")
        ),
    );
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    crate::license::note_nag_shown(snap.now_unix);
}

/// Open Settings on the Licence page (the licence balloon's click target).
fn open_licence_settings() {
    let tab = crate::settings_dlg::licence_page().to_string();
    let _ = super::spawn_self(&["--tab", &tab]);
}

extern "system" fn daemon_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_HOTKEY => {
                // Dispatched to the worker thread, not run inline — this wndproc
                // runs on the same thread that owns the WH_KEYBOARD_LL hook, and the
                // CreateProcess a hotkey triggers must never block it.
                dispatch_to_worker(DaemonWork::Hotkey(wparam.0 as i32));
                LRESULT(0)
            }
            // Quick preview Space hook (see `spacehook`): the hook callback posts these so the
            // heavier FindWindow / spawn / WM_COPYDATA work happens OFF the LL-hook callback —
            // and, off THIS thread too (it's the same thread as the hook), so it
            // goes to the worker rather than running here.
            m if m == super::spacehook::WM_APP_PREVIEW => {
                dispatch_to_worker(DaemonWork::PreviewToggle);
                LRESULT(0)
            }
            m if m == super::spacehook::WM_APP_PREVIEW_CLOSE => {
                dispatch_to_worker(DaemonWork::PreviewClose);
                LRESULT(0)
            }
            // A window we would have served just came to the foreground. If it is elevated,
            // Space is already dead there and the user has no way to find that out, so say it
            // now rather than letting them press a key that never reaches us.
            m if m == super::elevwarn::WM_APP_CHECK_ELEVATED => {
                on_check_elevated(hwnd, wparam);
                LRESULT(0)
            }
            WM_RELOAD => {
                on_reload(hwnd);
                LRESULT(0)
            }
            WM_TRAY => {
                on_tray(hwnd, lparam);
                LRESULT(0)
            }
            WM_TIMER => {
                on_timer(hwnd, wparam);
                LRESULT(0)
            }
            // Sleep/resume, lock/unlock, RDP reconnect and display changes can each silently drop
            // a live `RegisterHotKey` while this process stays up — so the hotkey quietly dies
            // while the process remains apparently healthy. Re-assert on each event so the
            // hotkey comes back the instant the machine does, with no "reopen the app" needed.
            WM_POWERBROADCAST => on_powerbroadcast(hwnd, wparam),
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
                on_taskbar_created(hwnd);
                LRESULT(0)
            }
            WM_UPDATE_FOUND => {
                on_update_found(hwnd, lparam);
                LRESULT(0)
            }
            WM_LICENCE_DUE => {
                on_licence_due(hwnd);
                LRESULT(0)
            }
            WM_COMMAND => {
                on_command(hwnd, wparam);
                LRESULT(0)
            }
            WM_DESTROY => {
                on_destroy(hwnd);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// `WM_HOTKEY` - dispatch by which of the three registered hotkey ids fired. Runs on the
/// worker thread, not the wndproc/hook thread — `spawn` is a `CreateProcess`.
unsafe fn on_hotkey(id: i32) {
    match id {
        HOTKEY_ID => spawn(Some("--screenshot")),
        QUICK_HOTKEY_ID => spawn(Some("--screenshot-instant")),
        CUSTOM_HOTKEY_ID => spawn(Some("--hotkey-action")),
        _ => {}
    }
}

/// `super::elevwarn::WM_APP_CHECK_ELEVATED`.
unsafe fn on_check_elevated(hwnd: HWND, wparam: WPARAM) {
    if let Some(kind) = super::elevwarn::warning_for(HWND(wparam.0 as *mut c_void)) {
        show_elevated_warning(hwnd, kind);
    }
}

/// `WM_RELOAD` - posted by the Settings window when the user picks a different capture
/// hotkey: re-read + re-register it, and reconcile the tray icon with the (possibly
/// just-changed) hide-tray setting.
unsafe fn on_reload(hwnd: HWND) {
    rearm_hotkeys(hwnd);
    if sagethumbs2k_core::settings::screenshot_hide_tray() {
        remove_tray_icon(hwnd);
    } else {
        ensure_tray_icon(hwnd);
    }
}

/// `WM_TRAY` - the notify-icon callback: double-click captures, right-click/context menu
/// opens the tray menu, and a balloon click routes on which balloon we last raised.
unsafe fn on_tray(hwnd: HWND, lparam: LPARAM) {
    let ev = (lparam.0 & 0xffff) as u32;
    if ev == WM_LBUTTONDBLCLK {
        spawn(Some("--screenshot"));
    } else if ev == WM_RBUTTONUP || ev == WM_CONTEXTMENU {
        show_tray_menu(hwnd);
    } else if ev == NIN_BALLOONUSERCLICK {
        // One message for every balloon, so route on which one we last raised.
        match LAST_BALLOON.swap(BALLOON_NONE, Ordering::Relaxed) {
            BALLOON_ELEVATED => spawn(None), // open Settings to bind a hotkey
            BALLOON_LICENCE => open_licence_settings(),
            _ => open_releases(), // the "update available" toast
        }
    }
}

/// `WM_TIMER` - dispatch by timer id: the periodic update check, the hotkey re-arm
/// backstop, and the tray-icon add retry (logon race).
unsafe fn on_timer(hwnd: HWND, wparam: WPARAM) {
    match wparam.0 {
        UPDATE_TIMER_ID => kick_update_check(hwnd),
        LICENCE_TIMER_ID => kick_licence_tick(hwnd),
        // Catch-all backstop: re-assert the hotkey registrations in case some
        // unforeseen event silently dropped them while we kept running.
        REARM_TIMER_ID => rearm_hotkeys(hwnd),
        // The taskbar rejected our icon earlier (logon race) - try again until
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
}

/// `WM_POWERBROADCAST` - only on RESUME (never on suspend, so we never release the
/// chord right before sleeping, which would leave it unregistered until wake).
unsafe fn on_powerbroadcast(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    let ev = wparam.0 as u32;
    if ev == PBT_APMRESUMEAUTOMATIC || ev == PBT_APMRESUMESUSPEND {
        rearm_hotkeys(hwnd);
    }
    LRESULT(1) // TRUE - grant the power-state change
}

/// The taskbar-created broadcast: re-add the tray icon (unless the user hid it).
unsafe fn on_taskbar_created(hwnd: HWND) {
    if !sagethumbs2k_core::settings::screenshot_hide_tray() {
        ensure_tray_icon(hwnd);
    }
}

/// `WM_UPDATE_FOUND`. `lparam` carries nothing (see [`UPDATE_TAG`]) — the tag is
/// read from the process-local slot, so a forged message (this class name is a fixed,
/// `FindWindowW`-discoverable constant, postable by any same-desktop process) just finds
/// nothing to show instead of us reconstructing-and-freeing an attacker-chosen pointer.
unsafe fn on_update_found(hwnd: HWND, _lparam: LPARAM) {
    let tag = UPDATE_TAG.lock().ok().and_then(|mut slot| slot.take());
    if let Some(tag) = tag {
        show_update_toast(hwnd, &tag);
    }
}

/// `WM_COMMAND` - the tray menu's item ids.
unsafe fn on_command(hwnd: HWND, wparam: WPARAM) {
    match wparam.0 & 0xffff {
        IDM_CAPTURE => spawn(Some("--screenshot")),
        IDM_OCR => spawn(Some("--screenshot-ocr")),
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
            // hotkey is bound - an explicit "stop everything".
            super::quit();
        }
        _ => {}
    }
}

/// `WM_DESTROY` - tear down timers, session notifications, hooks, tray icon and
/// hotkeys, then post the quit message.
unsafe fn on_destroy(hwnd: HWND) {
    let _ = KillTimer(Some(hwnd), UPDATE_TIMER_ID);
    let _ = KillTimer(Some(hwnd), REARM_TIMER_ID);
    let _ = WTSUnRegisterSessionNotification(hwnd);
    super::spacehook::uninstall(); // drop the Space hook with the daemon
    super::spacehook::reset_latch();
    super::elevwarn::uninstall(); // and its foreground watcher
    remove_tray_icon(hwnd);
    let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
    let _ = UnregisterHotKey(Some(hwnd), QUICK_HOTKEY_ID);
    let _ = UnregisterHotKey(Some(hwnd), CUSTOM_HOTKEY_ID);
    PostQuitMessage(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A140: the 60s re-arm timer must skip the settings write when the bitmask hasn't
    /// actually changed — otherwise a portable install rewrites its whole ini once a minute
    /// forever even while every hotkey stays bound.
    #[test]
    fn hotkey_bind_failed_changed_only_on_a_real_difference() {
        // Same value stored vs. computed → no write needed.
        assert!(!hotkey_bind_failed_changed(Some(0), 0));
        assert!(!hotkey_bind_failed_changed(Some(3), 3));
        // A genuine change → write needed.
        assert!(hotkey_bind_failed_changed(Some(0), 1));
        assert!(hotkey_bind_failed_changed(Some(3), 0));
        // Never-written (None) reads as 0 everywhere this value is consumed (see
        // settings_dlg/mod.rs's status line), so it must compare equal to a freshly
        // computed 0 — a brand-new daemon must not immediately write a redundant 0.
        assert!(!hotkey_bind_failed_changed(None, 0));
        assert!(hotkey_bind_failed_changed(None, 1));
    }
}
