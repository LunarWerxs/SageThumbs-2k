//! The scheduled update check: the task itself, and the one-shot run it triggers.

use super::*;

/// The update-check task's schedule: daily from 09:00, repeating every 6 h through the day.
const UPDATE_TRIGGER: &str = "    <CalendarTrigger>\n      \
     <StartBoundary>2026-01-01T09:00:00</StartBoundary>\n      \
     <Repetition>\n        <Interval>PT6H</Interval>\n        <Duration>P1D</Duration>\n      \
     </Repetition>\n      <Enabled>true</Enabled>\n      \
     <ScheduleByDay>\n        <DaysInterval>1</DaysInterval>\n      </ScheduleByDay>\n    \
     </CalendarTrigger>\n";

/// Register (or refresh) the per-user update-check task: `SageThumbs2K.exe --update-check`,
/// daily with a 6 h repetition, at the user's NORMAL token (the check writes only
/// `%LOCALAPPDATA%` and pops a tray balloon; it never needs admin). The 6 h cadence mirrors
/// what the resident helper does; the actual network hit stays throttled to once a day inside
/// [`check_throttled`], so the extra ticks only cover machines that were asleep. Registered
/// in-process ([`st2k_base::tasksched`]), never through `schtasks.exe`. Returns false if Task
/// Scheduler refused (policy) — the piggyback path then carries the feature on its own.
/// Best-effort with logging; never fatal.
pub(crate) fn install_update_task() -> bool {
    let (Ok(exe), Some(name)) = (
        std::env::current_exe(),
        st2k_base::tasksched::per_user_name(UPDATE_TASK),
    ) else {
        return false;
    };
    let xml = st2k_base::tasksched::exec_task_xml(
        "Checks once a day whether a newer SageThumbs 2K is out.",
        UPDATE_TRIGGER,
        &exe.to_string_lossy(),
        "--update-check",
        "PT72H",
    );
    match st2k_base::tasksched::register(&name, &xml) {
        Ok(()) => {
            // The one machine-wide task versions before per-user names registered.
            st2k_base::tasksched::delete_if_ours(UPDATE_TASK);
            true
        }
        Err(e) => {
            st2k_base::safety::log(&format!("update: could not register the update task: {e}"));
            false
        }
    }
}

/// Drop the update-check task (the user turned auto-check off, or we're uninstalling).
/// A missing task is not an error.
pub fn remove_update_task() {
    if let Some(name) = st2k_base::tasksched::per_user_name(UPDATE_TASK) {
        if let Err(e) = st2k_base::tasksched::delete(&name) {
            st2k_base::safety::log(&format!("update: could not remove the update task: {e}"));
        }
    }
    st2k_base::tasksched::delete_if_ours(UPDATE_TASK);
}

/// What [`sync_update_task`] does to the Scheduled Task.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TaskAction {
    Install,
    Remove,
    Leave,
}

/// Pure: a portable copy leaves the task alone. Registering one would leave a task on the
/// host pointing at a USB stick that is about to leave (the portable promise is that nothing
/// stays behind; the update cache sits beside its ini for the same reason), and removing one
/// would delete the task an installed copy on the same PC owns. Its checks ride its own
/// launches instead ([`spawn_due_check`]). Otherwise the task follows the setting.
pub(super) fn task_action(portable: bool, auto_check: bool) -> TaskAction {
    if portable {
        TaskAction::Leave
    } else if auto_check {
        TaskAction::Install
    } else {
        TaskAction::Remove
    }
}

/// Make the Scheduled Task match the "Automatically check for updates" setting. Called
/// after every install and whenever the Settings checkbox is applied, so turning the
/// setting off genuinely removes the task instead of leaving an inert one behind.
pub fn sync_update_task() {
    match task_action(
        st2k_base::settings::portable(),
        st2k_base::settings::update_auto_check(),
    ) {
        TaskAction::Install => {
            install_update_task();
        }
        TaskAction::Remove => remove_update_task(),
        TaskAction::Leave => {}
    }
}

/// Where a click on the "update available" balloon takes the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastClick {
    /// Settings, on the page with this nav key: Advanced holds the update button, Licence
    /// the renewal.
    Settings(&'static str),
    /// The releases page, where a portable copy gets its new zip.
    Releases,
}

/// The balloon's body key and click target. Pure, so every case is pinned by a test: a
/// portable copy is never told to "open SageThumbs 2K to install it" (it can't), and a click
/// always leads somewhere. It used to do nothing at all.
pub(super) fn toast_choice(offer: &Offer, portable: bool) -> (&'static str, ToastClick) {
    match offer {
        Offer::OutsideWindow { .. } => ("upd_outside_toast", ToastClick::Settings("nav_licence")),
        _ if portable => ("upd_toast_body_portable", ToastClick::Releases),
        _ => ("upd_toast_body", ToastClick::Settings("nav_advanced")),
    }
}

/// The "update available" balloon for `latest` on this machine: title, body, click target.
/// The updates-window decision is made here for every balloon (the scheduled one-shot's and
/// the resident helper's), so none promises an install this machine's licence will decline.
pub fn update_toast(latest: &LatestRelease) -> (&'static str, String, ToastClick) {
    let offer = offer_for(&crate::license::snapshot(), Some(latest));
    let (key, click) = toast_choice(&offer, st2k_base::settings::portable());
    let mut body = crate::win::t(key).replace("{ver}", &latest.tag);
    if let Offer::OutsideWindow { ends_unix } = offer {
        body = body.replace("{date}", &crate::license::format_unix_date(ends_unix));
    }
    (crate::win::t("upd_toast_title"), body, click)
}

/// Follow a balloon click.
pub fn open_toast_click(click: ToastClick) {
    match click {
        ToastClick::Settings(tab) => {
            let _ = crate::win::spawn_self(&["--tab", tab]);
        }
        ToastClick::Releases => unsafe { crate::win::open_url(RELEASES_URL) },
    }
}

/// `--update-check`: the one-shot the Scheduled Task (and [`spawn_due_check`]) runs. Honors
/// the user's auto-check setting, does one throttled check, and pops a non-blocking tray
/// balloon if a newer release exists. Silent when up to date, offline, or throttled.
pub fn run_one_shot_check() {
    // The licence tick rides this same daily one-shot: for most installs it is the only
    // thing that ever runs in the background, so it is where a business copy's evaluation
    // clock starts and where the reminder reaches a machine with no resident helper. It
    // runs BEFORE the update-check opt-out below - a licence is not an update - and it is
    // throttled and fail-open inside `background_tick` (a Personal copy pays one HKLM read).
    if let Some(snap) = crate::license::background_tick() {
        let body = format!(
            "{} {}",
            crate::license::licence_reminder_body(&snap),
            crate::win::t("licence_toast_enter_key")
        );
        unsafe {
            crate::win::notify_toast_action(
                crate::win::t("licence_popup_title"),
                &body,
                Duration::from_secs(10),
                move || {
                    if let Ok(exe) = std::env::current_exe() {
                        let _ = std::process::Command::new(exe)
                            .args(["--tab", "nav_licence"])
                            .spawn();
                    }
                },
            );
        }
        crate::license::note_nag_shown(snap.now_unix);
    }
    if !st2k_base::settings::update_auto_check() {
        return;
    }
    let Some(latest) = check_throttled() else {
        return;
    };
    // The window decision is made in `update_toast` as well as in the About card, because
    // this one-shot is the only thing many installs ever run.
    let (title, body, click) = update_toast(&latest);
    // Long enough to be seen and clicked: a click lands on the page that installs it.
    unsafe {
        crate::win::notify_toast_action(title, &body, Duration::from_secs(30), move || {
            open_toast_click(click)
        });
    }
}

/// Piggyback the update check on an ordinary app launch: if the once-a-day throttle has
/// expired, spawn the detached `--update-check` one-shot and return immediately. The caller
/// does no network work and can exit whenever it likes — the toast belongs to the child.
pub fn spawn_due_check() {
    if !st2k_base::settings::update_auto_check() || !check_due() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = std::process::Command::new(exe)
        .arg("--update-check")
        .creation_flags(st2k_base::host::CREATE_NO_WINDOW)
        .spawn();
}
