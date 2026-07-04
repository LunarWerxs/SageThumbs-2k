//! A tiny supervisor that keeps the screenshot/hotkey daemon alive (`--screenshot-watchdog`).
//!
//! The daemon ([`super::daemon`]) is the per-user helper that owns the global hotkeys.
//! Because the release build is `panic = "abort"`, ANY panic — or a hard crash, or a
//! Task-Manager kill — takes the whole daemon process down and silently loses every
//! `RegisterHotKey`, so the hotkeys just stop firing with no sign why. This watchdog
//! closes that gap: while the feature is still wanted, it restarts the daemon whenever
//! its window disappears.
//!
//! It is deliberately the simplest possible process — no hotkeys, no tray, no image
//! decoding, just `FindWindow` + `spawn` on a timer — so it has essentially nothing to
//! crash on itself. The daemon spawns it on startup (so it protects existing installs
//! immediately, with no autostart-entry change), and it exits on its own the moment
//! nothing wants the daemon (the HKCU autostart entry is gone — the user disabled every
//! dependent feature, or hit the tray "Quit").

use core::sync::atomic::{AtomicU32, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::*;

pub(super) const CLASS: PCWSTR = w!("SageThumbs2KShotWatchdog");

/// Health-check cadence while all is well (ms). Short enough that a dead daemon is back
/// within a couple of seconds; cheap enough that idling costs nothing.
const CHECK_MS: u32 = 2500;
/// Ceiling for the backoff a persistently-crashing daemon widens the check to, so a
/// crash loop can't become a respawn storm (it keeps trying, just slower).
const BACKOFF_MAX_MS: u32 = 60_000;
const CHECK_TIMER_ID: usize = 1;

/// Consecutive health checks that found the daemon missing — drives the crash-loop backoff.
static MISSES: AtomicU32 = AtomicU32::new(0);

pub(crate) unsafe fn run_watchdog(hinst: HINSTANCE) {
    // Single instance, TOCTOU-safe: named mutex first (the daemon's startup ensure and its
    // 5s re-ensure timer can both spawn a watchdog in the same instant — FindWindow alone
    // lets both through). Mirrors run_daemon; the handle is held for process life.
    let Ok(_lock) = CreateMutexW(None, true, w!("SageThumbs2K.ShotWatchdog.Single")) else {
        return;
    };
    if GetLastError() == ERROR_ALREADY_EXISTS {
        return;
    }
    // Belt-and-suspenders — one supervisor is enough.
    if FindWindowW(CLASS, PCWSTR::null()).is_ok() {
        return;
    }
    let wc = WNDCLASSW {
        lpfnWndProc: Some(watchdog_wndproc),
        hInstance: hinst,
        lpszClassName: CLASS,
        ..Default::default()
    };
    RegisterClassW(&wc);

    // A normal but never-shown window, just so the watchdog is single-instance
    // (FindWindow) and can be told to stop (WM_CLOSE), mirroring the daemon.
    let Ok(hwnd) = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        CLASS,
        w!("SageThumbs 2K Hotkey Watchdog"),
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

    // Check once right now (bring the daemon up immediately if it's missing), then poll.
    if !tick(hwnd) {
        return; // nothing wanted — tick already destroyed the window
    }
    let _ = SetTimer(Some(hwnd), CHECK_TIMER_ID, CHECK_MS, None);

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

/// One health check. Returns `false` once nothing wants the daemon (and destroys the
/// window so the loop stops). Otherwise (re)starts the daemon if its window is gone, and
/// widens the retry cadence if it keeps dying immediately (crash-loop guard).
unsafe fn tick(hwnd: HWND) -> bool {
    if !super::supervise_wanted() {
        let _ = DestroyWindow(hwnd);
        return false;
    }
    if super::is_daemon_running() {
        // Healthy — reset the backoff to the normal cadence if we'd widened it.
        if MISSES.swap(0, Ordering::Relaxed) != 0 {
            let _ = SetTimer(Some(hwnd), CHECK_TIMER_ID, CHECK_MS, None);
        }
    } else {
        super::spawn_self(&["--screenshot-daemon"]);
        let n = MISSES.fetch_add(1, Ordering::Relaxed) + 1;
        // A normal one-off restart rechecks at the healthy cadence; only a persistent
        // failure (the daemon keeps dying at once) widens the interval to avoid a storm.
        if n > 3 {
            let shift = (n - 3).min(5); // cap the doubling
            let ms = CHECK_MS.saturating_mul(1 << shift).min(BACKOFF_MAX_MS);
            let _ = SetTimer(Some(hwnd), CHECK_TIMER_ID, ms, None);
        }
    }
    true
}

extern "system" fn watchdog_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                if wparam.0 == CHECK_TIMER_ID {
                    tick(hwnd);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = KillTimer(Some(hwnd), CHECK_TIMER_ID);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
