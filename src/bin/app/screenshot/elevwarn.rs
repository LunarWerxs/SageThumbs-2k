//! Tell the user why "press Space to preview" is about to do nothing.
//!
//! This is the ONE failure the Space hook cannot report for itself. Windows withholds keystrokes
//! typed into an ELEVATED window from ordinary programs, and our hook is an ordinary program, so
//! the keypress never arrives at all: there is no swallowed key to notice, only silence. Measured
//! rather than assumed — a non-elevated `WH_KEYBOARD_LL` hook saw every key typed into a normal
//! window and NONE of the keys typed into an elevated one.
//!
//! So we do not wait for the key. We watch the FOREGROUND instead: the moment a window we would
//! have served becomes active while running elevated, we already know Space is dead there, and we
//! can say so before the user presses anything.
//!
//! Design rules, mirroring [`super::spacehook`]:
//!
//! - **The callback is tiny.** A `WINEVENT_OUTOFCONTEXT` hook is delivered on the daemon's own
//!   message queue, so a slow callback stalls the daemon. It reads the class name and posts;
//!   the token check (which opens a process handle) happens in the daemon's wndproc.
//! - **It NEVER goes permanently silent.** The first cut warned once per program per daemon
//!   lifetime, which is wrong: the user who presses Space, gets nothing, and presses it again is
//!   exactly the person the warning exists for, and they had already used up their one warning.
//!   We cannot see them trying — the keystrokes never arrive, and neither the input hook nor
//!   `GetAsyncKeyState` can see a key typed into an elevated window (both measured) — so the
//!   reminder repeats on a BACKOFF instead: quick while they are still fiddling, easing off to
//!   occasional, never off. See [`gap_for`].
//! - **Silent when the feature is off.** No Quick preview, no warning about Quick preview.

use core::ffi::c_void;
use core::sync::atomic::{AtomicIsize, AtomicU32, AtomicU64, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowThreadProcessId, PostMessageW, EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT, WM_APP,
};

/// Posted to the daemon when a window we serve came to the foreground. `wparam` carries its HWND;
/// the daemon does the elevation check and decides whether to warn.
pub(super) const WM_APP_CHECK_ELEVATED: u32 = WM_APP + 6;

static HOOK: AtomicIsize = AtomicIsize::new(0);
static DAEMON_HWND: AtomicIsize = AtomicIsize::new(0);

/// How long after a warning before the same program may warn again, the FIRST time. Short,
/// because someone who just got the warning and is still switching between windows is someone
/// still trying to make it work.
const WARN_FIRST_GAP_MS: u64 = 30_000;
/// The ceiling the backoff eases out to. Long enough to stop being noise for someone who has
/// decided to live with it, short enough that it is still a standing reminder rather than a
/// one-off they may have blinked and missed.
const WARN_MAX_GAP_MS: u64 = 30 * 60_000;

/// The quiet period owed after `count` previous warnings: 30 s, 1 m, 2 m, 4 m … capped at
/// [`WARN_MAX_GAP_MS`]. Doubling keeps the reminders close together while the user is actively
/// fiddling with the window, then eases off — WITHOUT ever reaching "never again".
fn gap_for(count: u32) -> u64 {
    WARN_FIRST_GAP_MS
        .saturating_mul(1u64 << count.min(6))
        .min(WARN_MAX_GAP_MS)
}

/// Warning bookkeeping for one program kind: when it last warned (`GetTickCount64`, 0 = never)
/// and how many times, which is what drives [`gap_for`].
struct WarnState {
    last_ms: AtomicU64,
    count: AtomicU32,
}

impl WarnState {
    const fn new() -> Self {
        Self {
            last_ms: AtomicU64::new(0),
            count: AtomicU32::new(0),
        }
    }

    /// Is a warning due now? Records it if so. Only ever called on the daemon's UI thread.
    fn due(&self) -> bool {
        // SAFETY: `GetTickCount64` reads a system counter; no pointers, no state of ours.
        let now = unsafe { GetTickCount64() };
        let last = self.last_ms.load(Ordering::Relaxed);
        let count = self.count.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < gap_for(count) {
            return false;
        }
        self.last_ms.store(now, Ordering::Relaxed);
        self.count.store(count.saturating_add(1), Ordering::Relaxed);
        true
    }

    /// The problem is gone (this program is running normally now), so the next time it DOES
    /// happen starts over at a prompt reminder instead of inheriting a 30-minute silence.
    fn reset(&self) {
        self.last_ms.store(0, Ordering::Relaxed);
        self.count.store(0, Ordering::Relaxed);
    }
}

static STATE_EXPLORER: WarnState = WarnState::new();
static STATE_EVERYTHING: WarnState = WarnState::new();

/// Install the foreground watcher when Quick preview is on; remove it otherwise. Idempotent, so
/// the daemon can call it from startup, `WM_RELOAD`, and its periodic re-arm, exactly like the
/// Space hook's own [`super::spacehook::rearm`].
pub(super) unsafe fn rearm(daemon_hwnd: HWND) {
    DAEMON_HWND.store(daemon_hwnd.0 as isize, Ordering::Relaxed);
    uninstall();
    if !sagethumbs2k_core::settings::preview_enabled() {
        return;
    }
    let hook = SetWinEventHook(
        EVENT_SYSTEM_FOREGROUND,
        EVENT_SYSTEM_FOREGROUND,
        None,
        Some(win_event_proc),
        0, // any process
        0, // any thread
        WINEVENT_OUTOFCONTEXT,
    );
    HOOK.store(hook.0 as isize, Ordering::Relaxed);
}

/// Remove the watcher if installed (called by [`rearm`] and on daemon teardown).
pub(super) unsafe fn uninstall() {
    let h = HOOK.swap(0, Ordering::Relaxed);
    if h != 0 {
        let _ = UnhookWinEvent(HWINEVENTHOOK(h as *mut c_void));
    }
}

/// Foreground changed. Cheap class check only, then hand off to the daemon.
unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    // OBJID_WINDOW / CHILDID_SELF only — ignore the per-control chatter.
    if id_object != 0 || id_child != 0 || hwnd.0.is_null() {
        return;
    }
    let raw = DAEMON_HWND.load(Ordering::Relaxed);
    if raw == 0 {
        return;
    }
    let cls = crate::explorer_selection::class_name(hwnd);
    if sagethumbs2k_core::doctor::served_window_kind(&cls).is_none() {
        return;
    }
    let _ = PostMessageW(
        Some(HWND(raw as *mut c_void)),
        WM_APP_CHECK_ELEVATED,
        WPARAM(hwnd.0 as usize),
        LPARAM(0),
    );
}

/// The daemon's half: is this foreground window actually out of reach, and have we said so?
///
/// Returns the program's name when the user should be warned, `None` otherwise. Opening a process
/// handle is far too heavy for the hook callback, which is why it lives here.
pub(super) unsafe fn warning_for(hwnd: HWND) -> Option<&'static str> {
    if !sagethumbs2k_core::settings::preview_enabled() {
        return None;
    }
    let cls = crate::explorer_selection::class_name(hwnd);
    let kind = sagethumbs2k_core::doctor::served_window_kind(&cls)?;
    let state = if kind == "Everything" {
        &STATE_EVERYTHING
    } else {
        &STATE_EXPLORER
    };
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if !sagethumbs2k_core::prebuild::process_is_elevated(pid) {
        // Fixed (or never broken): clear the backoff so a future relapse warns promptly again.
        state.reset();
        return None;
    }
    // An elevated window is only a problem for a NON-elevated listener. If the daemon somehow
    // got elevated too, keys reach it fine and there is nothing to warn about.
    if sagethumbs2k_core::prebuild::is_elevated() {
        return None;
    }
    state.due().then_some(kind)
}

// ---------------------------------------------------------------------------------------
// Why there is NO "just grab Space" trick here. Do not re-attempt this.
// ---------------------------------------------------------------------------------------
//
// The obvious idea is to `RegisterHotKey` on Space itself while an elevated window we serve is
// in front, since a hotkey is a different delivery path from an input hook and DOES survive
// UIPI. It was built and measured, and it does not work. The rule turns out to be:
//
//   * an UNMODIFIED hotkey (bare Space) is delivered like ordinary keyboard input, so UIPI
//     blocks it exactly as it blocks the hook: 0 of 3 presses arrived over an elevated window,
//     against 3 of 3 over a normal one;
//   * a MODIFIED combination (Ctrl+Space, Ctrl+Shift+F8) goes through the system's hotkey path
//     and DOES arrive over an elevated window: 3 of 3, both combinations.
//
// So Space ALONE can never reach us over an elevated window, now proven three independent ways
// (`WH_KEYBOARD_LL`, `GetAsyncKeyState`, and a bare-Space hotkey). The supported answer is the
// `act_preview` hotkey action, which the user binds to any combination WITH a modifier.

#[cfg(test)]
mod tests {
    use super::{gap_for, WARN_FIRST_GAP_MS, WARN_MAX_GAP_MS};
    use sagethumbs2k_core::doctor::served_window_kind;

    /// THE property this whole module exists for. The first version warned once and then went
    /// quiet forever, so a user who kept pressing Space never heard from us again. Whatever the
    /// backoff grows to, it must stay finite: there is always a next reminder.
    #[test]
    fn the_reminder_never_switches_itself_off() {
        for count in 0..1000u32 {
            let gap = gap_for(count);
            assert!(gap > 0, "a zero gap would spam");
            assert!(
                gap <= WARN_MAX_GAP_MS,
                "count {count} produced {gap} ms, past the cap — that is silence by another name"
            );
        }
    }

    /// The first warning is immediate (nothing recorded yet), then reminders start close
    /// together and only gradually spread out.
    #[test]
    fn the_backoff_starts_prompt_and_eases_off() {
        assert_eq!(gap_for(0), WARN_FIRST_GAP_MS); // 30 s to the second warning
        assert_eq!(gap_for(1), WARN_FIRST_GAP_MS * 2); // then a minute
        assert_eq!(gap_for(2), WARN_FIRST_GAP_MS * 4);
        assert!(gap_for(3) < gap_for(4), "it should keep spreading out");
        assert_eq!(gap_for(6), WARN_MAX_GAP_MS, "and then settle at the cap");
    }

    /// A doubling that overflows would wrap to a tiny gap and turn the reminder into a spammer.
    #[test]
    fn a_huge_count_cannot_wrap_into_spam() {
        assert_eq!(gap_for(u32::MAX), WARN_MAX_GAP_MS);
    }

    /// The watcher and `st2k doctor` share this list on purpose; if it ever stops covering the
    /// windows the Space hook serves, the warning goes quiet for exactly the case it exists for.
    #[test]
    fn the_served_classes_are_the_ones_space_actually_serves() {
        assert_eq!(served_window_kind("CabinetWClass"), Some("File Explorer"));
        assert_eq!(served_window_kind("ExploreWClass"), Some("File Explorer"));
        assert_eq!(served_window_kind("EVERYTHING"), Some("Everything"));
        assert_eq!(served_window_kind("EVERYTHING_(1.5a)"), Some("Everything"));
        assert_eq!(served_window_kind("Chrome_WidgetWin_1"), None);
        assert_eq!(served_window_kind("Progman"), None);
        assert_eq!(served_window_kind(""), None);
    }
}
