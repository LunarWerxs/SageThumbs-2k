//! Enabling/disabling the screenshot hotkey — the opt-in mechanism, kept out of
//! the UI so the Settings checkbox is a one-liner (`set_enabled`) and nothing
//! about the screenshot feature has to live in `settings_dlg.rs`.
//!
//! The resident tray daemon is wanted whenever EITHER the screenshot feature is on
//! OR a custom action hotkey is bound (see [`crate::hotkey`]) OR Quick preview is
//! enabled — so a colour-picker hotkey works without forcing the user to enable
//! screenshots. The autostart (a per-user sign-in task) therefore means
//! "the daemon should run", and the screenshot feature's
//! own on/off lives in its own `ScreenshotEnabled` DWORD (migrated from the old
//! "autostart-present == enabled" meaning). [`reconcile`] aligns the autostart
//! and the running daemon with whatever wants it. Default (nothing bound) = nothing
//! running, so the no-background-bloat promise holds until the user opts in.
//!
//! The autostart used to be a `…\Run` value naming this exe. Behaviour scanners read a
//! low-prevalence program writing itself into `Run`, then holding a keyboard hook, as a
//! dropper installing persistence: Kaspersky deleted the value as `Trojan-Dropper` (issue
//! #14), and VirusTotal's sandbox flags the write ("CurrentVersion Autorun Keys
//! Modification"). A sign-in task registered in-process starts the helper the same way
//! without that shape; an install that still has the value moves over on its next heal.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_CLOSE};

/// Where installs before the sign-in task kept the autostart: read for the migration and the
/// pre-`ScreenshotEnabled` fallback, otherwise only ever deleted.
const LEGACY_RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const LEGACY_RUN_NAME: &str = "SageThumbs2KScreenshot";
/// The helper's sign-in task is `SageThumbs2K_Helper_<user SID>`, one per user
/// ([`st2k_base::tasksched::per_user_name`]), so two accounts on one PC never fight over one.
const HELPER_TASK: &str = "SageThumbs2K_Helper";
/// Set by [`quit`], cleared by [`set_enabled`] (the single choke point every
/// Settings ▸ Save routes through, see `settings_dlg/values.rs`). While set, nothing else
/// wanting the daemon can bring it back — see [`daemon_wanted_from`].
const DAEMON_STOPPED_KEY: &str = "DaemonStopped";

/// Is the screenshot capture feature enabled? Stored as the `ScreenshotEnabled` DWORD.
/// For users upgrading from before that flag existed, fall back to the old `…\Run` value's
/// presence (which used to BE the screenshot-enabled state) so their setting migrates
/// cleanly — once `set_enabled` writes the DWORD, the fallback is never consulted again.
pub fn is_enabled() -> bool {
    match st2k_base::settings::get_dword_opt("ScreenshotEnabled") {
        Some(v) => v != 0,
        None => legacy_run_value_present(),
    }
}

/// Is a custom action hotkey bound (a non-disabled chord)? Such a binding also needs the
/// daemon resident, independently of the screenshot feature.
fn custom_hotkey_bound() -> bool {
    st2k_base::settings::custom_action_hotkey().1 != 0
}

/// Is Quick preview enabled? Its Space keyboard hook lives in this same daemon, so the
/// daemon must be resident whenever the feature is on — independently of screenshots or a
/// custom hotkey.
fn preview_wanted() -> bool {
    st2k_base::settings::preview_enabled()
}

/// Was the daemon explicitly stopped from the tray, and hasn't Settings been
/// saved since? See [`DAEMON_STOPPED_KEY`].
fn daemon_stopped() -> bool {
    st2k_base::settings::get_dword_opt(DAEMON_STOPPED_KEY) == Some(1)
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

/// True when something wants the daemon to survive logon, but the sign-in task isn't there
/// to make that happen — the registration in [`reconcile`] can fail (a policy, a security
/// product deleting it moments later, …) while THIS session's
/// daemon keeps running fine, so nothing else looks wrong until the next reboot, when the
/// hotkey/Quick-preview simply never comes back. Consulted by the daemon's tray tooltip so
/// the gap is visible somewhere the user will actually see it.
pub(crate) fn autostart_missing_while_wanted() -> bool {
    autostart_missing_while_wanted_from(daemon_wanted(), autostart_allowed(), !autostart_present())
}

/// Pure core of [`autostart_missing_while_wanted`]: the mismatch that means "something
/// deleted our autostart entry out from under us" is exactly settings-say-yes AND
/// we're-allowed-to-manage-it AND the value isn't there — a user who turned autostart off
/// (via `wanted=false`) or a portable copy (via `allowed=false`) has no mismatch at all, so
/// neither one should ever heal or nag. Split out so the decision is tested without a real
/// registry (see the `tests` module below).
fn autostart_missing_while_wanted_from(wanted: bool, allowed: bool, missing: bool) -> bool {
    wanted && allowed && missing
}

/// Whether we may touch logon autostart at all.
///
/// A portable copy never does. Its exe lives wherever the user unzipped it, so a sign-in
/// task would be persistent machine state from a build whose entire promise is that it
/// leaves none — and it would point at a path that dies the moment the folder is moved,
/// renamed, or unplugged, which is precisely the stale-autostart failure the guard in
/// [`autostart_points_at_other_install`] exists to clean up after. The daemon still runs
/// for the current session when something wants it; it just doesn't survive a logoff.
fn autostart_allowed() -> bool {
    !st2k_base::settings::portable()
}

/// This user's sign-in task name, or `None` when the token gives no SID.
fn helper_task() -> Option<String> {
    st2k_base::tasksched::per_user_name(HELPER_TASK)
}

/// Is the sign-in task registered? A file check, no COM
/// ([`st2k_base::tasksched::file_present`]): the tray tooltip asks from the thread that owns
/// the keyboard hook.
fn autostart_present() -> bool {
    helper_task().is_some_and(|task| st2k_base::tasksched::file_present(&task))
}

/// Does this user still have the pre-task `…\Run` value? (Once the "screenshots enabled"
/// signal, then "the daemon should autostart"; now only something to migrate away from.)
fn legacy_run_value_present() -> bool {
    windows_registry::CURRENT_USER
        .open(LEGACY_RUN_KEY)
        .and_then(|k| k.get_string(LEGACY_RUN_NAME))
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

/// Delete the pre-task `…\Run` value, if this user has one. `create`, not `open`: `open` is
/// read-only, the delete then fails with access denied, and the helper is left with two
/// autostarts (a dev build did exactly that).
fn remove_legacy_run_value() {
    if !legacy_run_value_present() {
        return;
    }
    let removed = windows_registry::CURRENT_USER
        .create(LEGACY_RUN_KEY)
        .and_then(|k| k.remove_value(LEGACY_RUN_NAME));
    if let Err(e) = removed {
        st2k_base::safety::log(&format!(
            "screenshot: could not remove the old autostart Run value: {e}"
        ));
    }
}

/// Does `task` start a live exe that ISN'T `current`? True means "leave the task alone" —
/// someone else's healthy install owns it (the usual case: the machine-wide install under
/// Program Files, while `current` is a dev/portable build). An absent task, or one whose
/// target is gone, returns false, i.e. rewrite freely — this exe beats a path that no longer
/// launches anything. Comparison is by canonical path, so a task that names *this* exe
/// through a different spelling still refreshes normally.
fn autostart_points_at_other_install(task: &str, current: &std::path::Path) -> bool {
    let Some(target) = st2k_base::tasksched::definition(task)
        .and_then(|xml| st2k_base::tasksched::command_of(&xml))
    else {
        return false;
    };
    match (
        std::path::Path::new(&target).canonicalize(),
        current.canonicalize(),
    ) {
        // Target exists and is genuinely a different file → it's someone's live install.
        (Ok(t), Ok(c)) => t != c,
        // Target missing/unreadable → stale, rewrite.
        _ => false,
    }
}

/// Is the tray daemon actually running right now (its hidden window exists)? The
/// hotkey only fires while it's alive, so the Settings status line reads this — a
/// stale autostart entry with no live daemon is the "set it but it doesn't fire" case.
pub fn is_daemon_running() -> bool {
    unsafe { FindWindowW(super::daemon::CLASS, PCWSTR::null()).is_ok() }
}

/// Self-heal on app launch: if the daemon is wanted (screenshots on OR a custom hotkey
/// bound OR Quick preview enabled) but nothing is running, bring it back — e.g. after
/// a crash/kill, or a logon where it never came up. Merely opening the app then
/// restarts the helper, matching the user's "if it's on, it should be running"
/// expectation. A no-op when already
/// running or not wanted.
pub fn heal_if_wanted() {
    let (count_heal, reconcile_now) = heal_plan(
        daemon_wanted(),
        autostart_allowed(),
        legacy_run_value_present(),
        autostart_present(),
        is_daemon_running(),
    );
    if count_heal {
        // Log BEFORE reconcile() registers the task again — a crash between the two would
        // rather leave a log line with no heal than a heal nobody can explain afterwards.
        record_autostart_heal();
    }
    if reconcile_now {
        reconcile();
    }
}

/// Pure core of [`heal_if_wanted`]: `(count and log a heal, reconcile)`. Three broken states,
/// and checking only the first one once missed a real case.
///
///   1. daemon not running -> crash, kill, or a logon where it never came up.
///   2. sign-in task gone while the daemon is STILL ALIVE. Antivirus does exactly this:
///      Kaspersky deleted the old `...\Run` value as "Trojan-Dropper" persistence (issue
///      #14) and left the process untouched. Nothing looked wrong until the next sign-in,
///      when the hotkey simply never came back. Counted, because it means something keeps
///      removing it.
///   3. an install from before the sign-in task still starts the helper from `...\Run`:
///      reconcile moves it over. A migration, not case 2, so never counted or logged as one.
fn heal_plan(
    wanted: bool,
    allowed: bool,
    legacy_run_value: bool,
    task_present: bool,
    running: bool,
) -> (bool, bool) {
    if !wanted {
        return (false, false);
    }
    let migrate = allowed && legacy_run_value;
    let missing = !migrate && autostart_missing_while_wanted_from(wanted, allowed, !task_present);
    (missing, !running || missing || migrate)
}

/// How many times [`heal_if_wanted`] has had to restore a missing sign-in task while
/// the daemon was still wanted. Persisted (not just logged) so the message can tell a
/// first-time heal from "this keeps happening" across separate launches/logons.
const AUTOSTART_HEAL_COUNT_KEY: &str = "AutostartHealCount";

/// Record + log a case-2 heal from [`heal_if_wanted`]. We still register the task again every
/// single time — refusing to would leave the user's hotkeys dead until they notice and open
/// Settings, which is worse than fighting the AV once per scan — so what's bounded here is
/// the LOG, not the heal: instead of an identical "healed" line forever (which reads, to a
/// support reader, exactly like a fix that never took), the count turns the second-and-later
/// occurrence into "this keeps happening", pointing at the real cause (a security product
/// repeatedly deleting the value) rather than at our code.
fn record_autostart_heal() {
    let prior = st2k_base::settings::get_dword_opt(AUTOSTART_HEAL_COUNT_KEY).unwrap_or(0);
    let _ = st2k_base::settings::set_dword(AUTOSTART_HEAL_COUNT_KEY, prior.saturating_add(1));
    if prior == 0 {
        st2k_base::safety::log(
            "screenshot: the helper's sign-in task was missing while the daemon is still \
             wanted (settings say autostart on, but the task is gone) — restoring it. \
             Likely cause: antivirus/cleanup software flagging it as persistence, as \
             Kaspersky did to the old Run value in issue #14. If this recurs, the user's \
             security software is the place to look — check its quarantine/threat log and \
             add an exclusion for our exe.",
        );
    } else {
        st2k_base::safety::log(&format!(
            "screenshot: the helper's sign-in task went missing AGAIN (heal #{}) — something \
             on this machine keeps deleting it, most likely antivirus real-time protection \
             re-flagging it on every scan. Restoring it again, but this is no longer a \
             one-off: point the user at their security product's exclusions/quarantine.",
            prior + 1
        ));
    }
}

/// Turn the screenshot capture feature on/off, then reconcile the daemon. Safe to call
/// repeatedly. (The daemon may still stay resident after `set_enabled(false)` if a custom
/// hotkey is bound — that's intentional; use [`quit`] for an unconditional stop.)
pub fn set_enabled(on: bool) {
    // Any Settings ▸ Save clears a tray "Quit" stop — the user is actively
    // engaging with Settings again, which is "re-enabling something" in the plainest sense.
    let _ = st2k_base::settings::set_dword(DAEMON_STOPPED_KEY, 0);
    let _ = st2k_base::settings::set_dword("ScreenshotEnabled", on as u32);
    reconcile();
}

/// Align the autostart entry + the running daemon with whether ANYTHING wants the daemon
/// (the screenshot feature OR a bound custom hotkey OR Quick preview enabled). Call after
/// any change to those settings: it adds/removes the autostart entry, starts a fresh daemon
/// (which reads the new settings on startup), or nudges an already-running one to
/// re-register. Safe to call
/// repeatedly.
pub(crate) fn reconcile() {
    if daemon_wanted() {
        reconcile_wanted();
    } else {
        reconcile_not_wanted();
    }
}

/// The `daemon_wanted()` branch of [`reconcile`]: make sure autostart points at this
/// install (when autostart is allowed at all) and that the daemon itself is up to date —
/// either nudged to re-read its hotkeys, or spawned fresh.
fn reconcile_wanted() {
    if autostart_allowed() {
        install_autostart_entry();
    }
    if is_daemon_running() {
        reload_hotkey(); // a live daemon re-reads + re-registers all hotkeys
    } else {
        st2k_appkit::win::spawn_self(&["--screenshot-daemon"]); // a fresh one reads them at startup
    }
}

/// Point the sign-in task at THIS exe — unless a healthy task already points at a
/// DIFFERENT install. Without that guard, merely opening Settings from a dev/test build
/// silently repointed logon autostart at a transient build path; when that path later
/// changed or vanished, the daemon simply never came up at the next boot (hotkeys dead, no
/// error anywhere) until something opened Settings again. Once a task is in, either one, the
/// old `…\Run` value (if any) goes: one autostart, never two.
fn install_autostart_entry() {
    let (Ok(exe), Some(sid)) = (
        std::env::current_exe(),
        st2k_base::tasksched::current_user_sid(),
    ) else {
        return;
    };
    let task = format!("{HELPER_TASK}_{sid}");
    if !autostart_points_at_other_install(&task, &exe) {
        let xml = st2k_base::tasksched::exec_task_xml(
            "Starts the SageThumbs 2K helper (hotkeys, Quick preview) when you sign in.",
            &st2k_base::tasksched::logon_trigger(&sid, "PT5S"),
            &exe.to_string_lossy(),
            "--screenshot-daemon",
            // No time limit: the helper is meant to run for the whole session.
            "PT0S",
        );
        // A failure here leaves hotkeys silently dead at the next logon — this session's
        // daemon starts fine regardless, so the log (and the tray tooltip) are the only signs.
        // The `…\Run` value stays: it is then the only autostart there is.
        if let Err(e) = st2k_base::tasksched::register(&task, &xml) {
            st2k_base::safety::log(&format!(
                "screenshot: failed to register the helper's sign-in task: {e}"
            ));
            return;
        }
    }
    migrate_legacy_run_value();
}

/// Retire the pre-task `…\Run` value now that a sign-in task starts the helper. While
/// `ScreenshotEnabled` was never written, [`is_enabled`] still reads that value as
/// "screenshots on", so the answer is written down first: deleting the value alone would
/// switch screenshots off for anyone who has not saved Settings since the DWORD arrived. If
/// it cannot be written, the value stays.
fn migrate_legacy_run_value() {
    if !legacy_run_value_present() {
        return;
    }
    if st2k_base::settings::get_dword_opt("ScreenshotEnabled").is_none()
        && st2k_base::settings::set_dword("ScreenshotEnabled", 1).is_err()
    {
        return;
    }
    remove_legacy_run_value();
}

/// Drop the sign-in task (and any old `…\Run` value), unless autostart isn't allowed at
/// all, in which case there is nothing of ours there. `action` names the caller in the
/// failure log line (`screenshot: {action} the helper's sign-in task: ...`).
fn remove_autostart_entry(action: &str) {
    if !autostart_allowed() {
        return;
    }
    if let Some(task) = helper_task() {
        if let Err(e) = st2k_base::tasksched::delete(&task) {
            st2k_base::safety::log(&format!(
                "screenshot: {action} the helper's sign-in task: {e}"
            ));
        }
    }
    remove_legacy_run_value();
}

/// Uninstall (`--remove-user-state`): drop this user's sign-in task and any old `…\Run`
/// value, touching no setting and no running process.
pub fn forget_autostart() {
    if let Some(task) = helper_task() {
        let _ = st2k_base::tasksched::delete(&task);
    }
    remove_legacy_run_value();
}

/// The "nothing wants it" branch of [`reconcile`]: drop the autostart entry (when autostart
/// is allowed at all) and close the daemon now.
fn reconcile_not_wanted() {
    remove_autostart_entry("failed to remove");
    unsafe { stop_daemon() };
}

/// Hard stop from the tray "Quit": turn screenshots off, drop the autostart entry, and close
/// the daemon now — regardless of any bound custom hotkey (an explicit "stop everything").
/// Sticky (see [`DAEMON_STOPPED_KEY`]): a bound custom hotkey or Quick preview
/// won't bring the daemon back on their own, at the next logon or any other ordinary launch —
/// only saving Settings again (which calls [`set_enabled`], clearing the stop, then
/// [`reconcile`]) does.
pub fn quit() {
    // Sticky across an ordinary launch — without this, `heal_if_wanted` (run from
    // several `main.rs` startup paths, including the background `--update-check` task) saw
    // `daemon_wanted()` still true (a bound custom hotkey or Quick preview keeps it true even
    // with screenshots off) and silently re-created the very autostart entry + daemon this
    // function just removed.
    let _ = st2k_base::settings::set_dword(DAEMON_STOPPED_KEY, 1);
    let _ = st2k_base::settings::set_dword("ScreenshotEnabled", 0);
    remove_autostart_entry("quit failed to remove");
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
pub fn reload_hotkey() {
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

    /// The Kaspersky/issue #14 signal: heal ONLY when the settings say yes, the registry says
    /// missing, and this is not a portable copy. A user who turned autostart off has
    /// `wanted=false` and so has no mismatch; a portable copy has `allowed=false` and must
    /// never get a Run entry healed back, which is `settings::portable`'s whole point (see
    /// [`autostart_allowed`]'s doc comment).
    #[test]
    fn autostart_missing_while_wanted_only_fires_on_the_real_mismatch() {
        assert!(autostart_missing_while_wanted_from(true, true, true));
        assert!(!autostart_missing_while_wanted_from(false, true, true));
        assert!(!autostart_missing_while_wanted_from(true, false, true));
        assert!(!autostart_missing_while_wanted_from(true, true, false));
        assert!(!autostart_missing_while_wanted_from(false, false, true));
        assert!(!autostart_missing_while_wanted_from(false, true, false));
        assert!(!autostart_missing_while_wanted_from(true, false, false));
        assert!(!autostart_missing_while_wanted_from(false, false, false));
    }

    /// Upgrading from an install that still starts the helper from the old `...\Run` value
    /// moves it to the sign-in task without counting a heal (which would log "antivirus keeps
    /// deleting it" to every upgrading user); a task that is genuinely gone is healed AND
    /// counted; a healthy, running helper is left alone; nothing happens when nothing wants
    /// the helper, or for a portable copy, which never has a task.
    #[test]
    fn heal_migrates_the_run_value_quietly_and_counts_only_real_losses() {
        // (wanted, allowed, legacy Run value, task present, running) -> (count, reconcile)
        assert_eq!(
            heal_plan(true, true, true, false, true),
            (false, true),
            "upgrade migrates"
        );
        assert_eq!(
            heal_plan(true, true, false, false, true),
            (true, true),
            "task deleted"
        );
        assert_eq!(
            heal_plan(true, true, false, true, true),
            (false, false),
            "healthy"
        );
        assert_eq!(
            heal_plan(true, true, false, true, false),
            (false, true),
            "not running"
        );
        assert_eq!(
            heal_plan(true, false, false, false, true),
            (false, false),
            "portable"
        );
        assert_eq!(
            heal_plan(false, true, true, false, false),
            (false, false),
            "not wanted"
        );
    }
}
