//! The scheduled update check: the task itself, and the one-shot run it triggers.

use super::*;

/// Run `schtasks.exe` with no console flash, returning its output.
pub(super) fn schtasks(args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new("schtasks.exe")
        .args(args)
        .creation_flags(sagethumbs2k_core::CREATE_NO_WINDOW)
        .output()
}

/// Register (or refresh) the per-user update-check task: `SageThumbs2K.exe --update-check`,
/// daily with a 6 h repetition, at the user's NORMAL token (`/rl LIMITED` — the check writes
/// only `%LOCALAPPDATA%` and pops a tray balloon; it never needs admin). The 6 h cadence
/// mirrors what the resident helper does; the actual network hit stays throttled to once a
/// day inside [`check_throttled`], so the extra ticks only cover machines that were asleep.
/// Returns false if `schtasks` refused (policy, missing binary) — the piggyback path then
/// carries the feature on its own. Best-effort with logging; never fatal.
pub(crate) fn install_update_task() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let tr = format!("\"{}\" --update-check", exe.display());
    #[rustfmt::skip]
    let created = schtasks(&["/create", "/f", "/tn", UPDATE_TASK, "/sc", "DAILY", "/st", "09:00",
                             "/ri", "360", "/du", "9999:59", "/rl", "LIMITED", "/tr", &tr]);
    match created {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            sagethumbs2k_core::safety::log(&format!(
                "update: schtasks create failed ({}): {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            ));
            false
        }
        Err(e) => {
            sagethumbs2k_core::safety::log(&format!("update: schtasks unavailable ({e})"));
            false
        }
    }
}

/// Drop the update-check task (the user turned auto-check off, or we're uninstalling).
/// A missing task is not an error.
pub(crate) fn remove_update_task() {
    let _ = schtasks(&["/delete", "/f", "/tn", UPDATE_TASK]);
}

/// Make the Scheduled Task match the "Automatically check for updates" setting. Called
/// after every install and whenever the Settings checkbox is applied, so turning the
/// setting off genuinely removes the task instead of leaving an inert one behind.
pub(crate) fn sync_update_task() {
    if sagethumbs2k_core::settings::update_auto_check() {
        install_update_task();
    } else {
        remove_update_task();
    }
}

/// `--update-check`: the one-shot the Scheduled Task (and [`spawn_due_check`]) runs. Honors
/// the user's auto-check setting, does one throttled check, and pops a non-blocking tray
/// balloon if a newer release exists. Silent when up to date, offline, or throttled.
pub(crate) fn run_one_shot_check() {
    // The licence tick rides this same daily one-shot: for most installs it is the only
    // thing that ever runs in the background, so it is where a business copy's evaluation
    // clock starts and where the reminder reaches a machine with no resident helper. It
    // runs BEFORE the update-check opt-out below - a licence is not an update - and it is
    // throttled and fail-open inside `background_tick` (a Personal copy pays one HKLM read).
    if let Some(snap) = crate::license::background_tick() {
        let body = format!(
            "{} {}",
            crate::settings_dlg::licence_reminder_body(&snap),
            crate::win::t("licence_toast_enter_key")
        );
        let tab = crate::settings_dlg::licence_page().to_string();
        unsafe {
            crate::win::notify_toast_action(
                crate::win::t("licence_popup_title"),
                &body,
                Duration::from_secs(10),
                move || {
                    if let Ok(exe) = std::env::current_exe() {
                        let _ = std::process::Command::new(exe)
                            .args(["--tab", &tab])
                            .spawn();
                    }
                },
            );
        }
        crate::license::note_nag_shown(snap.now_unix);
    }
    if !sagethumbs2k_core::settings::update_auto_check() {
        return;
    }
    let Some(latest) = check_throttled() else {
        return;
    };
    // The window decision is made HERE as well as in the About card, because this one-shot
    // is the only thing many installs ever run - the tray balloon must not promise an
    // install this machine's licence will then decline to perform.
    let snap = crate::license::snapshot();
    let body = match offer_for(&snap, Some(&latest)) {
        Offer::OutsideWindow { ends_unix } => crate::win::t("upd_outside_toast")
            .replace("{ver}", &latest.tag)
            .replace("{date}", &crate::settings_dlg::format_unix_date(ends_unix)),
        // `None` is unreachable with a `Some(..)` release, and treating it like Install is
        // the same "say nothing surprising" direction the rest of this module takes.
        Offer::Install | Offer::None => {
            crate::win::t("upd_toast_body").replace("{ver}", &latest.tag)
        }
    };
    unsafe {
        crate::win::notify_toast(
            crate::win::t("upd_toast_title"),
            &body,
            Duration::from_secs(8),
        );
    }
}

/// Piggyback the update check on an ordinary app launch: if the once-a-day throttle has
/// expired, spawn the detached `--update-check` one-shot and return immediately. The caller
/// does no network work and can exit whenever it likes — the toast belongs to the child.
pub(crate) fn spawn_due_check() {
    if !sagethumbs2k_core::settings::update_auto_check() || !check_due() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = std::process::Command::new(exe)
        .arg("--update-check")
        .creation_flags(sagethumbs2k_core::CREATE_NO_WINDOW)
        .spawn();
}
