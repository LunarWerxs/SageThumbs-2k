//! Enabling/disabling the screenshot hotkey — the opt-in mechanism, kept out of
//! the UI so the Settings checkbox is a one-liner (`set_enabled`) and nothing
//! about the screenshot feature has to live in `settings_dlg.rs`.
//!
//! The resident tray daemon is wanted whenever EITHER the screenshot feature is on
//! OR a custom action hotkey is bound (see [`crate::hotkey`]) — so a colour-picker
//! hotkey works without forcing the user to enable screenshots. The autostart entry
//! (`…\Run`) therefore means "the daemon should run", and the screenshot feature's
//! own on/off lives in its own `ScreenshotEnabled` DWORD (migrated from the old
//! "autostart-present == enabled" meaning). [`reconcile`] aligns the autostart entry
//! and the running daemon with whatever wants it. Default (nothing bound) = nothing
//! running, so the no-background-bloat promise holds until the user opts in.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_CLOSE};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "SageThumbs2KScreenshot";

/// Is the screenshot capture feature enabled? Stored as the `ScreenshotEnabled` DWORD.
/// For users upgrading from before that flag existed, fall back to the autostart entry's
/// presence (which used to BE the screenshot-enabled state) so their setting migrates
/// cleanly — once `set_enabled` writes the DWORD, the fallback is never consulted again.
pub(crate) fn is_enabled() -> bool {
    match sagethumbs2k_core::settings::get_dword_opt("ScreenshotEnabled") {
        Some(v) => v != 0,
        None => run_entry_present(),
    }
}

/// Is a custom action hotkey bound (a non-disabled chord)? Such a binding also needs the
/// daemon resident, independently of the screenshot feature.
fn custom_hotkey_bound() -> bool {
    sagethumbs2k_core::settings::custom_action_hotkey().1 != 0
}

/// Does the daemon need to be resident? True if screenshots are on OR a custom hotkey is bound.
fn daemon_wanted() -> bool {
    is_enabled() || custom_hotkey_bound()
}

/// Is the `…\Run` autostart entry present? (The legacy "screenshots enabled" signal, now
/// just "the daemon should autostart".)
fn run_entry_present() -> bool {
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

/// Turn the screenshot capture feature on/off, then reconcile the daemon. Safe to call
/// repeatedly. (The daemon may still stay resident after `set_enabled(false)` if a custom
/// hotkey is bound — that's intentional; use [`quit`] for an unconditional stop.)
pub(crate) fn set_enabled(on: bool) {
    let _ = sagethumbs2k_core::settings::set_dword("ScreenshotEnabled", on as u32);
    reconcile();
}

/// Align the autostart entry + the running daemon with whether ANYTHING wants the daemon
/// (the screenshot feature OR a bound custom hotkey). Call after any change to those
/// settings: it adds/removes the autostart entry, starts a fresh daemon (which reads the
/// new settings on startup), or nudges an already-running one to re-register. Safe to call
/// repeatedly.
pub(crate) fn reconcile() {
    if daemon_wanted() {
        if let (Ok(exe), Ok(k)) =
            (std::env::current_exe(), windows_registry::CURRENT_USER.create(RUN_KEY))
        {
            let _ = k.set_string(RUN_NAME, format!("\"{}\" --screenshot-daemon", exe.display()));
        }
        if is_daemon_running() {
            reload_hotkey(); // a live daemon re-reads + re-registers all hotkeys
        } else {
            super::spawn_self(&["--screenshot-daemon"]); // a fresh one reads them at startup
        }
    } else {
        if let Ok(k) = windows_registry::CURRENT_USER.create(RUN_KEY) {
            let _ = k.remove_value(RUN_NAME);
        }
        unsafe { stop_daemon() };
    }
}

/// Hard stop from the tray "Quit": turn screenshots off, drop the autostart entry, and close
/// the daemon now — regardless of any bound custom hotkey (an explicit "stop everything"). A
/// bound custom hotkey won't fire again until it's re-saved in Settings (which calls
/// [`reconcile`] and brings the daemon back).
pub(crate) fn quit() {
    let _ = sagethumbs2k_core::settings::set_dword("ScreenshotEnabled", 0);
    if let Ok(k) = windows_registry::CURRENT_USER.create(RUN_KEY) {
        let _ = k.remove_value(RUN_NAME);
    }
    unsafe { stop_daemon() };
}

/// Ask a running daemon to close (removes its tray icon + unregisters its hotkeys).
unsafe fn stop_daemon() {
    if let Ok(hwnd) = FindWindowW(super::daemon::CLASS, PCWSTR::null()) {
        let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

/// Tell a running daemon to re-read + re-register its hotkeys (after the user picks new
/// chords in Settings). No-op if the daemon isn't running — a fresh daemon reads the new
/// settings at startup anyway.
pub(crate) fn reload_hotkey() {
    unsafe {
        if let Ok(hwnd) = FindWindowW(super::daemon::CLASS, PCWSTR::null()) {
            let _ = PostMessageW(Some(hwnd), super::daemon::WM_RELOAD, WPARAM(0), LPARAM(0));
        }
    }
}
