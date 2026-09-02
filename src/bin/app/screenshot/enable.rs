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
/// Set by [`quit`], cleared by [`set_enabled`] (the single choke point every
/// Settings ▸ Save routes through, see `settings_dlg/values.rs`). While set, nothing else
/// wanting the daemon can bring it back — see [`daemon_wanted_from`].
const DAEMON_STOPPED_KEY: &str = "DaemonStopped";

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

/// Is Quick preview enabled? Its Space keyboard hook lives in this same daemon, so the
/// daemon must be resident whenever the feature is on — independently of screenshots or a
/// custom hotkey.
fn preview_wanted() -> bool {
    sagethumbs2k_core::settings::preview_enabled()
}

/// Was the daemon explicitly stopped from the tray, and hasn't Settings been
/// saved since? See [`DAEMON_STOPPED_KEY`].
fn daemon_stopped() -> bool {
    sagethumbs2k_core::settings::get_dword_opt(DAEMON_STOPPED_KEY) == Some(1)
}

/// Pure core of [`daemon_wanted`], split out for testing: an explicit stop
/// overrides every individual "something wants it" signal, so `quit()` cannot be silently
/// undone by the very next ordinary launch — `heal_if_wanted` runs from several `main.rs`
/// startup paths, including the background `--update-check` task, none of which should ever
/// re-arm autostart the user just removed on purpose.
fn daemon_wanted_from(stopped: bool, enabled: bool, custom_hotkey: bool, preview: bool) -> bool {
    !stopped && (enabled || custom_hotkey || preview)
}

/// Does the daemon need to be resident? True if screenshots are on OR a custom hotkey is
/// bound OR Quick preview is enabled — UNLESS the daemon was explicitly stopped and nothing
/// has re-enabled anything in Settings since.
fn daemon_wanted() -> bool {
    daemon_wanted_from(
        daemon_stopped(),
        is_enabled(),
        custom_hotkey_bound(),
        preview_wanted(),
    )
}

/// True when something wants the daemon to survive logon, but the `…\Run` autostart entry
/// isn't there to make that happen — the write in [`reconcile`] can fail silently (a
/// locked hive, an AV product deleting the value moments later, …) while THIS session's
/// daemon keeps running fine, so nothing else looks wrong until the next reboot, when the
/// hotkey/Quick-preview simply never comes back. Consulted by the daemon's tray tooltip so
/// the gap is visible somewhere the user will actually see it.
pub(crate) fn autostart_missing_while_wanted() -> bool {
    daemon_wanted() && autostart_allowed() && !run_entry_present()
}

/// Whether we may touch logon autostart at all.
///
/// A portable copy never does. Its exe lives wherever the user unzipped it, so a `…\Run`
/// entry would be persistent machine state from a build whose entire promise is that it
/// leaves none — and it would point at a path that dies the moment the folder is moved,
/// renamed, or unplugged, which is precisely the stale-autostart failure the guard in
/// [`autostart_points_at_other_install`] exists to clean up after. The daemon still runs
/// for the current session when something wants it; it just doesn't survive a logoff.
fn autostart_allowed() -> bool {
    !sagethumbs2k_core::settings::portable()
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

/// Does the existing autostart entry point at a live exe that ISN'T `current`? True means
/// "leave the entry alone" — someone else's healthy install owns it (the usual case: the
/// machine-wide install under Program Files, while `current` is a dev/portable build). An
/// absent, unparseable, or dead-target entry returns false, i.e. rewrite freely — this exe
/// beats a path that no longer launches anything. Comparison is by canonical path, so an
/// entry that names *this* exe through a different spelling still refreshes normally.
fn autostart_points_at_other_install(current: &std::path::Path) -> bool {
    let Ok(k) = windows_registry::CURRENT_USER.open(RUN_KEY) else {
        return false;
    };
    let Ok(v) = k.get_string(RUN_NAME) else {
        return false;
    };
    // Our own format is `"C:\path\to\exe" --screenshot-daemon` — take the quoted path.
    let rest = match v.trim().strip_prefix('"') {
        Some(r) => r,
        None => return false, // unquoted/foreign format — reclaim it
    };
    let Some(end) = rest.find('"') else {
        return false;
    };
    let target = std::path::Path::new(&rest[..end]);
    match (target.canonicalize(), current.canonicalize()) {
        // Target exists and is genuinely a different file → it's someone's live install.
        (Ok(t), Ok(c)) => t != c,
        // Target missing/unreadable → stale, rewrite.
        _ => false,
    }
}

/// Is the tray daemon actually running right now (its hidden window exists)? The
/// hotkey only fires while it's alive, so the Settings status line reads this — a
/// stale autostart entry with no live daemon is the "set it but it doesn't fire" case.
pub(crate) fn is_daemon_running() -> bool {
    unsafe { FindWindowW(super::daemon::CLASS, PCWSTR::null()).is_ok() }
}

/// Self-heal on app launch: if the daemon is wanted (screenshots on OR a custom hotkey
/// bound) but nothing is running, bring it back — e.g. after a crash/kill, or a logon
/// where it never came up. Merely opening the app then restarts the helper, matching
/// the user's "if it's on, it should be running" expectation. A no-op when already
/// running or not wanted.
pub(crate) fn heal_if_wanted() {
    if !daemon_wanted() {
        return;
    }
    // Two separate broken states, and checking only the first one missed a real case.
    //
    //   1. daemon not running  -> crash, kill, or a logon where it never came up.
    //   2. autostart entry gone while the daemon is STILL ALIVE. Antivirus does exactly this:
    //      Kaspersky deleted our `...\Run` value as "Trojan-Dropper" persistence (issue #14)
    //      and left the process untouched. Nothing looked wrong until the next sign-in, when
    //      the hotkey simply never came back, and the old `!is_daemon_running()` guard meant
    //      opening Settings could not repair it either.
    if !is_daemon_running() || (autostart_allowed() && !run_entry_present()) {
        reconcile();
    }
}

/// Turn the screenshot capture feature on/off, then reconcile the daemon. Safe to call
/// repeatedly. (The daemon may still stay resident after `set_enabled(false)` if a custom
/// hotkey is bound — that's intentional; use [`quit`] for an unconditional stop.)
pub(crate) fn set_enabled(on: bool) {
    // Any Settings ▸ Save clears a tray "Quit" stop — the user is actively
    // engaging with Settings again, which is "re-enabling something" in the plainest sense.
    let _ = sagethumbs2k_core::settings::set_dword(DAEMON_STOPPED_KEY, 0);
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
        // Point the autostart entry at THIS exe — unless a healthy entry already points at
        // a DIFFERENT install. Without that guard, merely opening Settings from a dev/test
        // build silently repointed logon autostart at a transient build path; when that
        // path later changed or vanished, the daemon simply never came up at the next boot
        // (hotkeys dead, no error anywhere) until something opened Settings again.
        if autostart_allowed() {
            if let Ok(exe) = std::env::current_exe() {
                if !autostart_points_at_other_install(&exe) {
                    match windows_registry::CURRENT_USER.create(RUN_KEY) {
                        Ok(k) => {
                            // A swallowed Err here left hotkeys silently dead at the
                            // next logon — this session's daemon starts fine regardless, so
                            // there was no other sign anything had gone wrong.
                            if let Err(e) = k.set_string(
                                RUN_NAME,
                                format!("\"{}\" --screenshot-daemon", exe.display()),
                            ) {
                                sagethumbs2k_core::safety::log(&format!(
                                    "screenshot: failed to write autostart Run entry: {e}"
                                ));
                            }
                        }
                        Err(e) => {
                            sagethumbs2k_core::safety::log(&format!(
                                "screenshot: failed to open Run key for autostart: {e}"
                            ));
                        }
                    }
                }
            }
        }
        if is_daemon_running() {
            reload_hotkey(); // a live daemon re-reads + re-registers all hotkeys
        } else {
            super::spawn_self(&["--screenshot-daemon"]); // a fresh one reads them at startup
        }
    } else {
        if autostart_allowed() {
            if let Ok(k) = windows_registry::CURRENT_USER.create(RUN_KEY) {
                if let Err(e) = k.remove_value(RUN_NAME) {
                    sagethumbs2k_core::safety::log(&format!(
                        "screenshot: failed to remove autostart Run entry: {e}"
                    ));
                }
            }
        }
        unsafe { stop_daemon() };
    }
}

/// Hard stop from the tray "Quit": turn screenshots off, drop the autostart entry, and close
/// the daemon now — regardless of any bound custom hotkey (an explicit "stop everything").
/// Sticky (see [`DAEMON_STOPPED_KEY`]): a bound custom hotkey or Quick preview
/// won't bring the daemon back on their own, at the next logon or any other ordinary launch —
/// only saving Settings again (which calls [`set_enabled`], clearing the stop, then
/// [`reconcile`]) does.
pub(crate) fn quit() {
    // Sticky across an ordinary launch — without this, `heal_if_wanted` (run from
    // several `main.rs` startup paths, including the background `--update-check` task) saw
    // `daemon_wanted()` still true (a bound custom hotkey or Quick preview keeps it true even
    // with screenshots off) and silently re-created the very autostart entry + daemon this
    // function just removed.
    let _ = sagethumbs2k_core::settings::set_dword(DAEMON_STOPPED_KEY, 1);
    let _ = sagethumbs2k_core::settings::set_dword("ScreenshotEnabled", 0);
    if autostart_allowed() {
        if let Ok(k) = windows_registry::CURRENT_USER.create(RUN_KEY) {
            if let Err(e) = k.remove_value(RUN_NAME) {
                sagethumbs2k_core::safety::log(&format!(
                    "screenshot: quit failed to remove autostart Run entry: {e}"
                ));
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Once `quit()` has set the stopped flag, no individual "something wants it"
    /// signal — screenshots on, a custom hotkey bound, Quick preview on, alone or combined —
    /// may bring the daemon back; only the flag being clear does. This is what stops
    /// `heal_if_wanted` (run from several `main.rs` startup paths, including the background
    /// `--update-check` task) from silently undoing a tray "Quit" on the very next ordinary
    /// launch, which was the bug: a bound custom hotkey kept the OLD `daemon_wanted()` true
    /// even with screenshots off.
    #[test]
    fn daemon_stopped_overrides_every_individual_want() {
        assert!(!daemon_wanted_from(true, true, true, true));
        assert!(!daemon_wanted_from(true, true, false, false));
        assert!(!daemon_wanted_from(true, false, true, false));
        assert!(!daemon_wanted_from(true, false, false, true));
        assert!(!daemon_wanted_from(true, false, false, false));
        // Not stopped: behaves exactly like the old OR-of-three-signals check.
        assert!(daemon_wanted_from(false, true, false, false));
        assert!(daemon_wanted_from(false, false, true, false));
        assert!(daemon_wanted_from(false, false, false, true));
        assert!(!daemon_wanted_from(false, false, false, false));
    }
}
