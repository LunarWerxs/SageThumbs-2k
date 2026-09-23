//! The `WH_KEYBOARD_LL` "press Space to preview" hook (Quick preview, Phase 2).
//!
//! Installed by the daemon ONLY while `preview_enabled()` (off by default). Design rules,
//! all load-bearing (see the plan §3) — do not relax them:
//!
//! - **Observe-only.** The callback NEVER swallows a key: it always falls through to
//!   `CallNextHookEx` and never returns 1. Space still reaches Explorer (checkbox-select mode
//!   toggles the box too — the same accepted overlap QuickLook has). This keeps the AV profile
//!   friendly (we block nothing) and avoids orphaned key-up states.
//! - **Tiny + no blocking.** A slow low-level hook (>~300 ms) is silently UNHOOKED by Windows,
//!   so the callback does only cheap user32 calls + a `PostMessageW` to the daemon. NO COM, NO
//!   file I/O, NO decode, NO window creation. The daemon's re-arm timer reinstalls us if
//!   Windows ever drops the hook.
//! - **Latch the down-tick.** A qualifying Space key-down latches "this press is ours" + the
//!   time; auto-repeat key-downs are ignored (debounce); the key-up uses the LATCHED verdict,
//!   never a re-check of the (possibly changed) foreground window.
//! - **This is NOT a keylogger.** It reads only the vk of the event, looks at Space/Esc/Enter,
//!   and posts a message. It captures no text, logs nothing, sends nothing anywhere.
//!
//! The callback runs on the DAEMON's UI thread (LL hooks fire on the installing thread, which
//! must pump messages — the daemon does), so the state below is only ever touched there.

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_MENU, VK_RETURN, VK_RWIN, VK_SHIFT,
    VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, FindWindowExW, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId,
    PostMessageW, SetWindowsHookExW, UnhookWindowsHookEx, GUITHREADINFO, HHOOK, KBDLLHOOKSTRUCT,
    WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_SYSKEYDOWN,
};

/// Posted to the daemon window on a qualifying Space press (toggle the preview).
pub(super) const WM_APP_PREVIEW: u32 = WM_APP + 4;
/// Posted on Esc / Enter / a hold-to-peek release (close the preview if it's open).
pub(super) const WM_APP_PREVIEW_CLOSE: u32 = WM_APP + 5;

/// Hold Space at least this long, then release = "peek" (close on release). Mirrors
/// QuickLook's `HOLD_TO_PREVIEW_DURATION`.
const HOLD_PEEK_MS: u64 = 750;

static HOOK: AtomicIsize = AtomicIsize::new(0);
static DAEMON_HWND: AtomicIsize = AtomicIsize::new(0);
static SPACE_LATCHED: AtomicBool = AtomicBool::new(false);
static SPACE_DOWN_TICK: AtomicU64 = AtomicU64::new(0);
/// Cached `preview_hold_peek()` — read ONCE in [`rearm`] (never from inside the hook
/// callback), because a registry read can block on I/O and a slow LL-hook callback gets
/// silently unhooked by Windows. Refreshed on every re-arm (startup / WM_RELOAD / the 60 s
/// timer), so it's at most 60 s stale.
static HOLD_PEEK: AtomicBool = AtomicBool::new(true);

/// Install the hook when Quick preview is enabled; remove it otherwise. Idempotent
/// (uninstall-then-install), so the daemon can call this at startup, on `WM_RELOAD` (the
/// setting just flipped), and from its power/session/display/60 s re-arm paths — the same
/// recovery discipline the RegisterHotKey bindings get, because Windows can silently drop a
/// slow LL hook too.
pub(super) unsafe fn rearm(daemon_hwnd: HWND) {
    DAEMON_HWND.store(daemon_hwnd.0 as isize, Ordering::Relaxed);
    // Cache the hold-to-peek setting here (on the daemon thread), NOT in the hook callback.
    HOLD_PEEK.store(st2k_base::settings::preview_hold_peek(), Ordering::Relaxed);
    uninstall();
    if !st2k_base::settings::preview_enabled() {
        reset_latch();
    }
    if st2k_base::settings::preview_enabled() {
        let hmod = GetModuleHandleW(None).ok();
        let hinst = hmod
            .map(|m| windows::Win32::Foundation::HINSTANCE(m.0))
            .unwrap_or_default();
        if let Ok(h) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), Some(hinst), 0) {
            HOOK.store(h.0 as isize, Ordering::Relaxed);
        }
    }
}

/// Remove the hook if installed (called by [`rearm`] and on daemon teardown). The
/// hold-to-peek latch is deliberately NOT touched here: the 60 s backstop timer, resume, a
/// session change and a display change all unhook and immediately reinstall, often while
/// Space is physically held, and the latch is process state the new hook instance reads
/// just as well, so the eventual key-up still closes the preview. Clearing it on every
/// re-arm made that release a no-op and left the preview open.
pub(super) unsafe fn uninstall() {
    let h = HOOK.swap(0, Ordering::Relaxed);
    if h != 0 {
        let _ = UnhookWindowsHookEx(HHOOK(h as *mut c_void));
    }
}

/// Forget a held Space. Only for the cases where no hook will see the key-up: the preview
/// feature being switched off, and daemon teardown. Otherwise the stale latch would make
/// the next press read as an auto-repeat and drop it.
pub(super) fn reset_latch() {
    SPACE_LATCHED.store(false, Ordering::Relaxed);
    SPACE_DOWN_TICK.store(0, Ordering::Relaxed);
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // HC_ACTION (0) = a real key event; anything < 0 must be passed straight through.
    if code == 0 {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let m = wparam.0 as u32;
        let is_down = is_key_down_message(m);
        handle_key(kb.vkCode, is_down);
    }
    // ALWAYS fall through — never swallow a key.
    CallNextHookEx(None, code, wparam, lparam)
}

/// Which of our trigger keys a virtual-key code is. The pure half of the dispatch in
/// [`handle_key`]: Space has its own latch/peek handling, Esc and Enter are the same close
/// action, and everything else is passed straight through untouched.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Trigger {
    Space,
    Close,
    None,
}

fn trigger_key(vk: u32) -> Trigger {
    if vk == VK_SPACE.0 as u32 {
        Trigger::Space
    } else if vk == VK_ESCAPE.0 as u32 || vk == VK_RETURN.0 as u32 {
        Trigger::Close
    } else {
        Trigger::None
    }
}

/// The key-up decision: a release after at least [`HOLD_PEEK_MS`] of a held Space closes the
/// preview ("hold to peek"), but only while the setting is on. Pure, so the boundary is
/// testable without the Win32 tick counter.
fn release_is_peek(held_ms: u64, hold_peek: bool) -> bool {
    held_ms >= HOLD_PEEK_MS && hold_peek
}

/// Whether a `WM_*` message id is a key-down (as opposed to a key-up or an unrelated event).
/// The pure half of the callback's classification.
fn is_key_down_message(m: u32) -> bool {
    m == WM_KEYDOWN || m == WM_SYSKEYDOWN
}

/// The (fast) per-key logic. Posts to the daemon; never blocks.
unsafe fn handle_key(vk: u32, is_down: bool) {
    let raw = DAEMON_HWND.load(Ordering::Relaxed);
    if raw == 0 {
        return;
    }
    let daemon = HWND(raw as *mut c_void);
    let key = trigger_key(vk);

    if key == Trigger::Space {
        handle_space_key(daemon, is_down);
    } else if key == Trigger::Close && is_down && qualifies() {
        // Esc / Enter close the preview if it's up. We never swallow, so Explorer still gets the
        // key too (Enter then opens the file natively — the intended hand-off).
        let _ = PostMessageW(Some(daemon), WM_APP_PREVIEW_CLOSE, WPARAM(0), LPARAM(0));
    }
}

/// The Space half of [`handle_key`]: latch a qualifying press (debouncing auto-repeat) and post
/// the open, or honour a key-up release as the hold-to-peek close. `daemon` is the post target.
unsafe fn handle_space_key(daemon: HWND, is_down: bool) {
    if is_down {
        if SPACE_LATCHED.load(Ordering::Relaxed) {
            return; // auto-repeat while held — debounce (QuickLook's _spaceIsDown)
        }
        if qualifies() {
            SPACE_LATCHED.store(true, Ordering::Relaxed);
            SPACE_DOWN_TICK.store(GetTickCount64(), Ordering::Relaxed);
            let _ = PostMessageW(Some(daemon), WM_APP_PREVIEW, WPARAM(0), LPARAM(0));
        }
    } else if SPACE_LATCHED.swap(false, Ordering::Relaxed) {
        // Key-up decision uses the LATCHED verdict (QuickLook keeps the down-time judgment).
        let held = GetTickCount64().saturating_sub(SPACE_DOWN_TICK.load(Ordering::Relaxed));
        if release_is_peek(held, HOLD_PEEK.load(Ordering::Relaxed)) {
            let _ = PostMessageW(Some(daemon), WM_APP_PREVIEW_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

/// Whether the current moment qualifies for a Space/Esc/Enter action: the foreground is
/// Explorer / the Desktop / our own viewer, no modifier is held, and the user is not typing.
/// All cheap user32 calls — safe in the LL-hook callback.
unsafe fn qualifies() -> bool {
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return false;
    }
    if modifier_down() {
        return false;
    }
    if !foreground_qualifies(fg) {
        return false;
    }
    if is_typing(fg) {
        return false;
    }
    true
}

/// Classes that qualify with no further probe: an Explorer folder window, or our own viewer
/// (so Space closes the preview). The other classes still need a child-window / result-list
/// probe and stay in [`foreground_qualifies`].
fn class_is_directly_qualified(cls: &str) -> bool {
    matches!(
        cls,
        "CabinetWClass" | "ExploreWClass" | "SageThumbs2KViewer"
    )
}

/// The foreground window class must be an Explorer view, the Desktop, an Everything search
/// window, or our viewer. Same dispatch QuickLook uses (`Shell32.cpp::GetFocusedWindowType`).
unsafe fn foreground_qualifies(fg: HWND) -> bool {
    let cls = crate::explorer_selection::class_name(fg);
    if class_is_directly_qualified(&cls) {
        return true;
    }
    match cls.as_str() {
        "Progman" | "WorkerW" => has_defview(fg), // the Desktop (has a SHELLDLL_DefView child)
        // A common Open/Save dialog. `is_typing` below still holds the file-name box, which
        // has the caret whenever the dialog opens — Space only becomes a preview once the
        // user has clicked into the item view.
        "#32770" => crate::dialog_hook::is_file_dialog(fg),
        // Everything (voidtools). BOTH gates, and in this order: the class stem is the only
        // stable part of its name, and only a build we can actually READ the focused result
        // from can answer the Space we are about to act on. Anything else and Space stays a
        // space. (`is_typing` below still holds the search box — its `Edit` reports a caret.)
        //
        // Two sources, because 1.5 and 1.4 differ: 1.5 publishes a hidden focus window, 1.4
        // publishes nothing and is read out of its `SysListView32` instead. Both probes are
        // plain `FindWindowEx` calls that send NO message, so this stays safe in an LL-hook
        // callback; the cross-process reads happen later, on the daemon thread.
        _ => {
            crate::explorer_selection::is_everything_class(&cls)
                && (crate::explorer_selection::everything_focus_window(fg).is_some()
                    || crate::explorer_selection::everything_result_list(fg).is_some())
        }
    }
}

/// True if `fg` hosts a `SHELLDLL_DefView` child — the desktop-icon view.
unsafe fn has_defview(fg: HWND) -> bool {
    FindWindowExW(Some(fg), None, w!("SHELLDLL_DefView"), PCWSTR::null()).is_ok()
}

/// Any of Ctrl / Shift / Alt / Win physically held right now (Space+modifier keeps its normal
/// meaning and is never our trigger).
unsafe fn modifier_down() -> bool {
    let down = |vk: i32| (GetAsyncKeyState(vk) as u16 & 0x8000) != 0;
    down(VK_CONTROL.0 as i32)
        || down(VK_SHIFT.0 as i32)
        || down(VK_MENU.0 as i32)
        || down(VK_LWIN.0 as i32)
        || down(VK_RWIN.0 as i32)
}

/// Whether the user is typing in the foreground window (F2 rename, address bar, IME, or the
/// UWP-hosted Explorer search box). QuickLook's exact check
/// (`HelperMethods.cpp::IsCursorActivated` + `IsExplorerSearchBoxFocused`): ask
/// `GetGUIThreadInfo` about the FOREGROUND window's OWN thread (never thread 0 — that's our
/// daemon, which always reports "not typing").
unsafe fn is_typing(fg: HWND) -> bool {
    let tid = GetWindowThreadProcessId(fg, None);
    let mut gti = GUITHREADINFO {
        cbSize: core::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    if GetGUIThreadInfo(tid, &mut gti).is_ok() {
        // Any active caret / menu / move-size / IME state, or a live caret window.
        if gti.flags.0 != 0 || !gti.hwndCaret.0.is_null() {
            return true;
        }
        // The Explorer search box is a UWP CoreWindow with no classic caret.
        if !gti.hwndFocus.0.is_null()
            && crate::explorer_selection::class_name(gti.hwndFocus) == "Windows.UI.Core.CoreWindow"
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::WM_KEYUP;

    /// Space is the preview trigger, Esc and Enter are the same "close" action, and every
    /// other key must be ignored — the hook only ever acts on these three vks.
    #[test]
    fn trigger_key_maps_space_close_and_ignores_everything_else() {
        assert_eq!(trigger_key(VK_SPACE.0 as u32), Trigger::Space);
        assert_eq!(trigger_key(VK_ESCAPE.0 as u32), Trigger::Close);
        assert_eq!(trigger_key(VK_RETURN.0 as u32), Trigger::Close);
        assert_eq!(trigger_key(b'A' as u32), Trigger::None);
        assert_eq!(trigger_key(0), Trigger::None);
    }

    /// Only `WM_KEYDOWN` / `WM_SYSKEYDOWN` are presses: a key-up (or any other message) must
    /// not be misread as a fresh press, or the latch would never release.
    #[test]
    fn only_key_down_messages_are_presses() {
        assert!(is_key_down_message(WM_KEYDOWN));
        assert!(is_key_down_message(WM_SYSKEYDOWN));
        assert!(!is_key_down_message(WM_KEYUP));
    }

    /// The hold-to-peek boundary: exactly `HOLD_PEEK_MS` counts, one tick less does not, and
    /// turning the setting off suppresses the close at every duration.
    #[test]
    fn peek_fires_only_at_the_hold_boundary_and_when_enabled() {
        assert!(release_is_peek(HOLD_PEEK_MS, true));
        assert!(release_is_peek(HOLD_PEEK_MS + 1, true));
        assert!(!release_is_peek(HOLD_PEEK_MS - 1, true));
        assert!(!release_is_peek(0, true));
        assert!(!release_is_peek(HOLD_PEEK_MS, false));
        assert!(!release_is_peek(u64::MAX, false));
    }

    /// The literal class-name table: the two Explorer folder windows and our own viewer
    /// qualify with no further probe, while the desktop, a file dialog and Everything must
    /// NOT short-circuit here (each still needs its own child-window / result-list probe).
    #[test]
    fn only_folder_and_viewer_classes_qualify_directly() {
        assert!(class_is_directly_qualified("CabinetWClass"));
        assert!(class_is_directly_qualified("ExploreWClass"));
        assert!(class_is_directly_qualified("SageThumbs2KViewer"));
        assert!(!class_is_directly_qualified("Progman"));
        assert!(!class_is_directly_qualified("WorkerW"));
        assert!(!class_is_directly_qualified("#32770"));
        assert!(!class_is_directly_qualified(""));
        assert!(
            !class_is_directly_qualified("cabinetwclass"),
            "class names are case-sensitive"
        );
    }
}
