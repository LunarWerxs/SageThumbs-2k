//! Load/save/apply settings + diagnostics actions (extracted from settings_dlg; parent-hub pattern).

use super::*;
mod apply;
mod load;
use apply::*;
mod hotkeys;
#[cfg(test)]
use hotkeys::*;
mod dependents;
mod maintenance;
pub(super) use apply::apply_settings;
#[cfg(test)]
pub(super) use apply::apply_tuning_numbers;
pub(super) use dependents::{
    badge_size_active, is_dependent_switch, sync_dependent_switches, update_badge_style_enabled,
    update_custom_action_enabled, update_quick_enabled, update_save_dir_enabled,
};
pub(super) use hotkeys::block_on_hotkey_conflict;
#[cfg(test)]
pub(super) use hotkeys::{
    conflicting_hotkeys, hotkey_conflict_decision, HotkeyBinding, HotkeyRole,
};
#[cfg(test)]
pub(super) use load::{
    custom_action_hk_combo_index, preset_combo_index, quick_hotkey_combo_index,
    shot_tool_combo_index,
};
pub(super) use load::{
    load_defaults, load_values, refresh_from_settings, reset_formats, seed_format_state,
    shot_delay_combo_index,
};
pub(super) use maintenance::{
    export_settings_to_file, import_settings_from_file, msg, open_diagnostics_log,
    rebuild_thumbnail_cache, repair_associations, reregister_elevated, spawn_cache_rebuild,
    CacheRebuiltEvent, Reg, WM_APP_CACHE,
};

/// A combo's current selection, clamped into `0..=max` so a control that does not exist (or an
/// empty one, which reports `CB_ERR` = -1) reads as the first option rather than as garbage.
pub(super) unsafe fn combo_sel(hwnd: HWND, id: i32, max: i32) -> i32 {
    match GetDlgItem(Some(hwnd), id) {
        Ok(c) => SendMessageW(c, CB_GETCURSEL, None, None).0 as i32,
        Err(_) => 0,
    }
    .clamp(0, max)
}

/// The Quick-preview blocklist edit box's current text, raw (untrimmed of a trailing NUL from
/// the Win32 buffer, trimmed here) — parsing happens on READ elsewhere
/// (`settings::preview_blocked`), so this is intentionally just the literal control text.
unsafe fn blocked_exts_text(hwnd: HWND) -> String {
    match GetDlgItem(Some(hwnd), ID_PREVIEW_BLOCKED_EXTS) {
        Ok(c) => String::from_utf16_lossy(&control_text(c))
            .trim_end_matches('\0')
            .to_string(),
        Err(_) => String::new(),
    }
}

/// Select `sel` in a combo, if that combo exists on this dialog.
pub(super) unsafe fn set_combo(hwnd: HWND, id: i32, sel: usize) {
    if let Ok(c) = GetDlgItem(Some(hwnd), id) {
        SendMessageW(c, CB_SETCURSEL, Some(WPARAM(sel)), None);
    }
}

pub(super) unsafe fn banner_rotator(hwnd: HWND) -> Option<(HWND, *mut SponsorRotator)> {
    let banner = GetDlgItem(Some(hwnd), ID_BANNER).ok()?;
    let rot = GetWindowLongPtrW(banner, GWLP_USERDATA) as *mut SponsorRotator;
    (!rot.is_null()).then_some((banner, rot))
}

thread_local! {
    /// Sticky within one [`apply_settings`] call: set the moment any tracked write fails and
    /// never cleared again until the next `apply_settings` resets it, so a later successful
    /// write can't paper over an earlier failure.
    static SAVE_FAILED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
mod combo_reseed_tests;
#[cfg(test)]
mod hotkey_conflict_tests;
