//! Enabling/disabling the screenshot hotkey — the opt-in mechanism, kept out of
//! the UI so the Settings checkbox is a one-liner (`set_enabled`) and nothing
//! about the screenshot feature has to live in `settings_dlg.rs`.
//!
//! "Enabled" = an HKCU `…\Run` autostart entry (so the tray daemon starts at
//! logon) **plus** the daemon running now. Disabling removes the autostart entry
//! and tells the running daemon to quit. Default (no entry) = nothing running, so
//! the no-background-bloat promise holds until the user opts in.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_CLOSE};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "SageThumbs2KScreenshot";

/// Is the screenshot hotkey enabled (autostart entry present)?
pub(crate) fn is_enabled() -> bool {
    windows_registry::CURRENT_USER
        .open(RUN_KEY)
        .and_then(|k| k.get_string(RUN_NAME))
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

/// Is the tray daemon actually running right now (its hidden window exists)? The
/// hotkey only fires while it's alive, so the Settings status line reads this — a
/// stale autostart entry with no live daemon is the "set it but it doesn't fire" case.
pub(crate) fn is_daemon_running() -> bool {
    unsafe { FindWindowW(super::daemon::CLASS, PCWSTR::null()).is_ok() }
}

/// Turn the screenshot hotkey on/off: manage the autostart entry and the live
/// daemon. Safe to call repeatedly.
pub(crate) fn set_enabled(on: bool) {
    if on {
        let Ok(exe) = std::env::current_exe() else { return };
        if let Ok(k) = windows_registry::CURRENT_USER.create(RUN_KEY) {
            let _ = k.set_string(RUN_NAME, format!("\"{}\" --screenshot-daemon", exe.display()));
        }
        // Start it now too (the daemon is single-instance, so a double-start no-ops).
        super::spawn_self(&["--screenshot-daemon"]);
    } else {
        if let Ok(k) = windows_registry::CURRENT_USER.create(RUN_KEY) {
            let _ = k.remove_value(RUN_NAME);
        }
        unsafe { stop_daemon() };
    }
}

/// Ask a running daemon to close (removes its tray icon + unregisters the hotkey).
unsafe fn stop_daemon() {
    if let Ok(hwnd) = FindWindowW(super::daemon::CLASS, PCWSTR::null()) {
        let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

/// Tell a running daemon to re-read + re-register its capture hotkey (after the
/// user picks a new one in Settings). No-op if the daemon isn't running — a fresh
/// daemon reads the new hotkey at startup anyway.
pub(crate) fn reload_hotkey() {
    unsafe {
        if let Ok(hwnd) = FindWindowW(super::daemon::CLASS, PCWSTR::null()) {
            let _ = PostMessageW(Some(hwnd), super::daemon::WM_RELOAD, WPARAM(0), LPARAM(0));
        }
    }
}
