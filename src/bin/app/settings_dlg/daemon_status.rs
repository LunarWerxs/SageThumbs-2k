//! The screenshot hotkey daemon's live status line (with its green/red tint state) and
//! the portable per-user Explorer registration toggle + its status word. Split out of
//! `mod.rs`.

use super::*;

// Whether the screenshot-daemon status line currently reads as a healthy "running" state,
// i.e. whether WM_CTLCOLORSTATIC should tint it green. Mirrors sync.rs's STATUS_GREEN: this
// used to be decided by scanning the control's text for "Running"/"Started", which only
// works while the line is hard-coded English: the moment it goes through `t()` the tint
// would silently stop matching in every translation, with nothing to fail (a green badge
// quietly turning grey is invisible to every test we have). Recording the state here, at
// the point the text is set, means the tint can't drift from what was actually written.
thread_local! {
    pub(super) static SHOT_STATUS_GREEN: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

pub(super) unsafe fn set_shot_status(hwnd: HWND, txt: &str, running: bool) {
    SHOT_STATUS_GREEN.with(|g| g.set(running));
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_SHOT_STATUS) {
        let w = wide(txt);
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
    }
}

/// Is this portable copy's per-user thumbnail registration pointing at the DLL sitting next
/// to THIS exe? Shared with the first-run welcome, which offers the same switch.
fn portable_registered_here() -> bool {
    sagethumbs2k_core::register::user_registration_is_here()
}

/// Refresh the portable registration button + its status word to match reality.
pub(super) unsafe fn set_portable_reg_state(hwnd: HWND) {
    let on = portable_registered_here();
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_PORTABLE_REG_STATUS) {
        let w = wide(t(if on { "state_on" } else { "state_off" }));
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
    }
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_PORTABLE_REG) {
        let w = wide(t(if on {
            "btn_portable_unregister"
        } else {
            "btn_portable_register"
        }));
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
    }
}

/// Turn the per-user Explorer registration on or off, then re-read the real state rather
/// than assuming the write landed.
pub(super) unsafe fn toggle_portable_registration(hwnd: HWND) {
    let on = portable_registered_here();
    let result = if on {
        sagethumbs2k_core::register::unregister_user().map(|_| ())
    } else {
        match sagethumbs2k_core::register::dll_beside_exe() {
            Some(dll) if dll.exists() => {
                sagethumbs2k_core::register::register_user(&dll.to_string_lossy())
            }
            // The DLL travels in the portable zip; if it is missing the copy was unpacked
            // partially or pruned, and saying which file beats a bare "failed".
            _ => {
                st2k_appkit::win::message_box(
                    hwnd,
                    t("msg_portable_dll_missing"),
                    t("btn_portable_register"),
                );
                return;
            }
        }
    };
    if result.is_err() {
        st2k_appkit::win::message_box(
            hwnd,
            t("msg_portable_reg_failed"),
            t("btn_portable_register"),
        );
    }
    set_portable_reg_state(hwnd);
}

/// Update the save-folder display (ID_SHOT_DIR) to the effective folder (the configured
/// one, or the Desktop default). Called on load and after the folder picker.
pub(super) unsafe fn set_shot_dir_label(hwnd: HWND) {
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_SHOT_DIR) {
        let w = wide(
            &t("shot_dir_label")
                .replace("{dir}", &st2k_screenshot::screenshot::effective_save_dir()),
        );
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
    }
}

/// Refresh the screenshot daemon status line from the live state.
pub(super) unsafe fn refresh_shot_status(hwnd: HWND) {
    use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
    // Drive off the LIVE "Enable screenshot hotkey" checkbox (not the persisted state),
    // so toggling it updates the status line + Restart button immediately.
    let enabled = checked(hwnd, ID_SHOT_ENABLE);
    // The daemon reports per-chord RegisterHotKey failures via the HotkeyBindFailed
    // bitmask (bit0 capture, bit1 quick-save, bit2 custom action) — a chord grabbed by
    // another app otherwise looked identical to a working one ("Running" while the
    // hotkey silently never fires). Only trust the flag while the daemon is actually
    // alive (it rewrites the mask on every re-arm; a dead daemon's value is stale).
    let bind_failed = if st2k_screenshot::screenshot::is_daemon_running() {
        settings::get_dword_opt("HotkeyBindFailed").unwrap_or(0)
    } else {
        0
    };
    let daemon_running = enabled && st2k_screenshot::screenshot::is_daemon_running();
    // Localized like every other line on the page: these were hard-coded English until
    // 2026-09-18, so a Chinese UI read "Running" beside translated labels. The tint never
    // depends on the text (see SHOT_STATUS_GREEN), so any language is safe here.
    let key = if !enabled {
        // Screenshot feature off — but a bound CUSTOM action hotkey still runs through
        // the same daemon, and ITS conflict (bit2) would otherwise be invisible in the
        // whole UI (this is the only status line).
        if bind_failed & 4 != 0 {
            "shot_status_off_conflict"
        } else {
            "state_off"
        }
    } else if daemon_running {
        if bind_failed != 0 {
            "shot_status_running_conflict"
        } else {
            "shot_status_running"
        }
    } else {
        "shot_status_stopped"
    };
    // Green exactly when the daemon is actually confirmed running: a bind conflict still
    // shows the running text (the daemon IS up) but the color question is the same either way.
    set_shot_status(hwnd, t(key), daemon_running);
    // The Restart button does nothing when the hotkey is off — disable + repaint it.
    if let Ok(btn) = GetDlgItem(Some(hwnd), ID_SHOT_RESTART) {
        let _ = EnableWindow(btn, enabled);
        let _ = InvalidateRect(Some(btn), None, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `set_shot_status` must record the green/not-green state in `SHOT_STATUS_GREEN`
    /// regardless of the control lookup outcome: the WM_CTLCOLORSTATIC tint handler reads
    /// that cell, not the window's (eventually localized) text, so the color can no longer
    /// drift out of step with a translation the way the old `txt.contains("Running")` sniff
    /// did (it matched only literal English). `HWND::default()` (no real control exists)
    /// exercises exactly the state-write half of the function, independent of GDI.
    #[test]
    fn set_shot_status_records_green_state_independent_of_the_label_text() {
        unsafe {
            set_shot_status(HWND::default(), "some string, in any language", true);
            assert!(
                SHOT_STATUS_GREEN.with(|g| g.get()),
                "running=true must set the tint cell"
            );
            set_shot_status(HWND::default(), "some string, in any language", false);
            assert!(
                !SHOT_STATUS_GREEN.with(|g| g.get()),
                "running=false must clear the tint cell, even though the text is unchanged"
            );
        }
    }
}
