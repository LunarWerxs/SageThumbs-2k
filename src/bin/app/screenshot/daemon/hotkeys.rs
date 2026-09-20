//! Registering the configured global hotkeys, and re-arming them when a bind fails.

use super::*;

/// (Re-)register the global capture hotkey from the persisted setting, converting
/// the stored HOTKEYF_* modifiers to RegisterHotKey's MOD_* flags. Best-effort.
/// Convert stored HOTKEYF_* modifier bits (SHIFT 0x01, CONTROL 0x02, ALT 0x04) to
/// RegisterHotKey's MOD_* flags, always with MOD_NOREPEAT so a held chord fires once.
pub(super) fn hkf_to_mods(hkf: u32) -> HOT_KEY_MODIFIERS {
    let mut mods = MOD_NOREPEAT;
    if hkf & 0x01 != 0 {
        mods |= MOD_SHIFT;
    }
    if hkf & 0x02 != 0 {
        mods |= MOD_CONTROL;
    }
    if hkf & 0x04 != 0 {
        mods |= MOD_ALT;
    }
    mods
}

/// (Re-)register BOTH the main capture hotkey and the optional quick-save hotkey
/// from the persisted settings. Best-effort — if a chord is taken the tray menu
/// still works. The quick hotkey is skipped when its vk is 0 (disabled).
pub(super) unsafe fn register_configured_hotkey(hwnd: HWND) {
    // Registration stays best-effort (a taken chord must not stop the tray/daemon), but
    // the failures are no longer INVISIBLE: a bitmask of which bindings failed (bit0
    // capture, bit1 quick-save, bit2 custom action) is persisted so the Settings status
    // line can say "hotkey in use by another app" instead of a flat "Running". Every
    // re-arm rewrites it, so releasing the conflicting app self-clears within a minute.
    let mut failed = 0u32;
    // The capture + quick-save hotkeys belong to the SCREENSHOT feature — register them only
    // while it's enabled, so a daemon kept alive solely for a custom hotkey doesn't also grab
    // Ctrl+PrtScn.
    if super::super::is_enabled() {
        let (hkf, vk) = sagethumbs2k_core::settings::screenshot_hotkey();
        if RegisterHotKey(Some(hwnd), HOTKEY_ID, hkf_to_mods(hkf), vk).is_err() {
            failed |= 1;
        }
        let (qhkf, qvk) = sagethumbs2k_core::settings::screenshot_quick_hotkey();
        if qvk != 0 && RegisterHotKey(Some(hwnd), QUICK_HOTKEY_ID, hkf_to_mods(qhkf), qvk).is_err()
        {
            failed |= 2;
        }
    }
    // The user-assignable custom action hotkey — independent of the screenshot feature.
    let (chkf, cvk) = sagethumbs2k_core::settings::custom_action_hotkey();
    if cvk != 0 && RegisterHotKey(Some(hwnd), CUSTOM_HOTKEY_ID, hkf_to_mods(chkf), cvk).is_err() {
        failed |= 4;
    }
    // A140: this fires from REARM_TIMER_ID every 60s for as long as the daemon runs, so an
    // unconditional write here is a full settings rewrite (a portable-mode ini rewrite) once a
    // minute forever, even on the overwhelming majority of ticks where nothing changed. Only
    // write when the bitmask actually moved.
    let current = sagethumbs2k_core::settings::get_dword_opt("HotkeyBindFailed");
    if hotkey_bind_failed_changed(current, failed) {
        let _ = sagethumbs2k_core::settings::set_dword("HotkeyBindFailed", failed);
    }
}

/// Whether the freshly-computed `HotkeyBindFailed` bitmask differs from what's already
/// stored, so [`register_configured_hotkey`] can skip the rewrite when it hasn't changed.
/// `current` mirrors [`sagethumbs2k_core::settings::get_dword_opt`]'s "absent" semantics —
/// the Settings status line (`settings_dlg/mod.rs`) reads a never-written value as `0`, so
/// `None` must compare equal to `new == 0` here too, or a freshly-installed daemon would
/// write a redundant `0` on its very first re-arm tick.
pub(super) fn hotkey_bind_failed_changed(current: Option<u32>, new: u32) -> bool {
    current.unwrap_or(0) != new
}

/// Drop and re-create every global hotkey registration from the current settings. Called by the
/// periodic backstop timer, the power/session/display re-arm triggers, and [`WM_RELOAD`] after a
/// settings change. Unregister-then-register is idempotent: if a binding is still live this is a
/// harmless no-op churn; if it was silently lost (sleep/resume, unlock, RDP reconnect …) this is
/// what brings it back — without the user having to reopen the app.
pub(super) unsafe fn rearm_hotkeys(hwnd: HWND) {
    let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
    let _ = UnregisterHotKey(Some(hwnd), QUICK_HOTKEY_ID);
    let _ = UnregisterHotKey(Some(hwnd), CUSTOM_HOTKEY_ID);
    register_configured_hotkey(hwnd);
    // The Quick preview Space hook rides the SAME recovery discipline: reinstall it (or remove
    // it if the feature was just turned off in Settings via WM_RELOAD). Windows can silently
    // drop a slow LL hook across sleep/resume/session-change, exactly like a RegisterHotKey.
    super::super::spacehook::rearm(hwnd);
    // And the watcher that explains the one failure that hook CANNOT report: an elevated
    // foreground window, whose keystrokes never reach us at all (see `elevwarn`).
    super::super::elevwarn::rearm(hwnd);
}
