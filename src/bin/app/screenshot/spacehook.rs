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
    GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_MENU, VK_RETURN, VK_RWIN, VK_SHIFT, VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, FindWindowExW, GetClassNameW, GetForegroundWindow, GetGUIThreadInfo,
    GetWindowThreadProcessId, PostMessageW, SetWindowsHookExW, UnhookWindowsHookEx, GUITHREADINFO,
    HHOOK, KBDLLHOOKSTRUCT, WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_SYSKEYDOWN,
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
    HOLD_PEEK.store(sagethumbs2k_core::settings::preview_hold_peek(), Ordering::Relaxed);
    uninstall();
    if sagethumbs2k_core::settings::preview_enabled() {
        let hmod = GetModuleHandleW(None).ok();
        let hinst = hmod.map(|m| windows::Win32::Foundation::HINSTANCE(m.0)).unwrap_or_default();
        if let Ok(h) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), Some(hinst), 0) {
            HOOK.store(h.0 as isize, Ordering::Relaxed);
        }
    }
}

/// Remove the hook if installed (called by [`rearm`] and on daemon teardown).
pub(super) unsafe fn uninstall() {
    let h = HOOK.swap(0, Ordering::Relaxed);
    if h != 0 {
        let _ = UnhookWindowsHookEx(HHOOK(h as *mut c_void));
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // HC_ACTION (0) = a real key event; anything < 0 must be passed straight through.
    if code == 0 {
        let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let m = wparam.0 as u32;
        let is_down = m == WM_KEYDOWN || m == WM_SYSKEYDOWN;
        handle_key(kb.vkCode, is_down);
    }
    // ALWAYS fall through — never swallow a key.
    CallNextHookEx(None, code, wparam, lparam)
}

/// The (fast) per-key logic. Posts to the daemon; never blocks.
unsafe fn handle_key(vk: u32, is_down: bool) {
    let raw = DAEMON_HWND.load(Ordering::Relaxed);
    if raw == 0 {
        return;
    }
    let daemon = HWND(raw as *mut c_void);
    let space = VK_SPACE.0 as u32;
    let esc = VK_ESCAPE.0 as u32;
    let enter = VK_RETURN.0 as u32;

    if vk == space {
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
            if held >= HOLD_PEEK_MS && HOLD_PEEK.load(Ordering::Relaxed) {
                let _ = PostMessageW(Some(daemon), WM_APP_PREVIEW_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    } else if is_down && (vk == esc || vk == enter) && qualifies() {
        // Esc / Enter close the preview if it's up. We never swallow, so Explorer still gets the
        // key too (Enter then opens the file natively — the intended hand-off).
        let _ = PostMessageW(Some(daemon), WM_APP_PREVIEW_CLOSE, WPARAM(0), LPARAM(0));
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

/// The foreground window class must be an Explorer view, the Desktop, or our viewer. Same
/// dispatch QuickLook uses (`Shell32.cpp::GetFocusedWindowType`).
unsafe fn foreground_qualifies(fg: HWND) -> bool {
    match class_name(fg).as_str() {
        "CabinetWClass" | "ExploreWClass" => true, // an Explorer folder window
        "SageThumbs2KViewer" => true,              // our own viewer (so Space closes it)
        "Progman" | "WorkerW" => has_defview(fg),  // the Desktop (has a SHELLDLL_DefView child)
        _ => false,
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
        if !gti.hwndFocus.0.is_null() && class_name(gti.hwndFocus) == "Windows.UI.Core.CoreWindow" {
            return true;
        }
    }
    false
}

/// The window class name (best-effort; empty on failure).
unsafe fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = GetClassNameW(hwnd, &mut buf);
    if n <= 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&buf[..n as usize])
    }
}
