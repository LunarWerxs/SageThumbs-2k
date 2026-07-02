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

/// Should the watchdog keep the daemon alive? Tied to the autostart entry — the canonical
/// "the daemon is wanted" signal that [`reconcile`]/[`quit`] add and remove. Using the
/// entry (not `daemon_wanted()` directly) means the watchdog stops cleanly after a tray
/// "Quit" even while a custom hotkey is still bound (Quit removes the entry).
pub(crate) fn supervise_wanted() -> bool {
    run_entry_present()
}

/// Make sure the [`watchdog`](super::watchdog) supervisor is running (spawns it if its
/// window isn't found). Single-instance, so a redundant call is a cheap no-op. Called by
/// the daemon on startup and by [`reconcile`] so a dead watchdog is re-established.
pub(crate) fn ensure_watchdog() {
    unsafe {
        if FindWindowW(super::watchdog::CLASS, PCWSTR::null()).is_err() {
            super::spawn_self(&["--screenshot-watchdog"]);
        }
    }
}

/// Ask a running watchdog to exit (so it stops respawning the daemon). Used when the
/// daemon is being torn down for good ([`quit`] / disabling every dependent feature).
unsafe fn stop_watchdog() {
    if let Ok(hwnd) = FindWindowW(super::watchdog::CLASS, PCWSTR::null()) {
        let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

/// Is the tray daemon actually running right now (its hidden window exists)? The
/// hotkey only fires while it's alive, so the Settings status line reads this — a
/// stale autostart entry with no live daemon is the "set it but it doesn't fire" case.
pub(crate) fn is_daemon_running() -> bool {
    unsafe { FindWindowW(super::daemon::CLASS, PCWSTR::null()).is_ok() }
}

/// Self-heal on app launch: if the daemon is wanted (screenshots on OR a custom hotkey
/// bound) but nothing is running, bring it back. Covers the case where BOTH the daemon and
/// its watchdog are down — e.g. after both were killed, or a logon where the daemon never
/// came up — which the watchdog alone can't recover (it isn't running either). Merely
/// opening the app then restarts the service, matching the user's "if it's on, it should be
/// running" expectation. A no-op when it's already running or not wanted.
pub(crate) fn heal_if_wanted() {
    if daemon_wanted() && !is_daemon_running() {
        reconcile();
    }
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
        ensure_watchdog(); // (re)establish the supervisor that restarts the daemon if it dies
    } else {
        if let Ok(k) = windows_registry::CURRENT_USER.create(RUN_KEY) {
            let _ = k.remove_value(RUN_NAME);
        }
        // Stop the watchdog FIRST so it can't respawn the daemon we're about to close.
        unsafe {
            stop_watchdog();
            stop_daemon();
        }
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
    // Stop the watchdog FIRST (removing the Run entry above already makes it exit on its
    // next tick, but stop it now so it can't respawn the daemon during the race).
    unsafe {
        stop_watchdog();
        stop_daemon();
    }
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
