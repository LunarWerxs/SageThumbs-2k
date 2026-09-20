//! WM_COMMAND: every button, link and menu id the dialog answers, routed by page.

use super::*;

/// WM_COMMAND (button clicks / menu picks / control notifications) and WM_NOTIFY (list
/// custom-draw, drag-reorder, tooltips) — plus the format list's context menu, which is
/// keyed off the same target control as the format list's other notifications.
pub(super) unsafe fn on_command_or_notify_msg(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    match msg {
        WM_COMMAND => Some(on_command(hwnd, wparam)),
        // A footer SysLink or the banner tooltip is asking for its rotating text.
        WM_NOTIFY => Some(on_notify(hwnd, lparam)),
        // Right-click / Shift+F10 on the format list → bulk check/uncheck menu.
        WM_CONTEXTMENU
            if HWND(wparam.0 as *mut c_void)
                == GetDlgItem(Some(hwnd), ID_LIST).unwrap_or_default() =>
        {
            list::list_context_menu(HWND(wparam.0 as *mut c_void), hwnd, lparam);
            Some(LRESULT(0))
        }
        _ => None,
    }
}

pub(super) unsafe fn on_command(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    let (id, notify) = crate::win::command_parts(wparam);
    on_command_dialog(hwnd, id, notify);
    on_command_shot(hwnd, id);
    on_command_sync_nav(hwnd, id, notify);
    on_command_admin(hwnd, id);
    on_command_licence(hwnd, id);
    LRESULT(0)
}

/// Core dialog chrome: Save/Cancel, the file-type list's bulk toggles + search + reset,
/// and the menu-items list's reset/editor.
pub(super) unsafe fn on_command_dialog(hwnd: HWND, id: i32, notify: u32) {
    match id {
        IDOK => {
            // Refuse the whole Save when two enabled hotkeys share a chord, naming both,
            // rather than writing a duplicate whose LATER-registered half will silently
            // never fire (2026-09-05 audit, F27). See `block_on_hotkey_conflict`.
            if !block_on_hotkey_conflict(hwnd) {
                apply_settings(hwnd); // Save = apply only, keep the window open
                spawn_sync_push(hwnd); // if signed in, mirror the change to the cloud
            }
        }
        IDCANCEL => close_settings(hwnd),
        ID_SELECT_ALL | ID_CLEAR_ALL => on_select_clear_all(hwnd, id),
        // Settings-wide search: filter on every keystroke, jump on pick.
        ID_SEARCH_GLOBAL if notify == EN_CHANGE => search::on_change(hwnd),
        ID_SEARCH_RESULTS if notify == LBN_SELCHANGE => search::on_pick(hwnd),
        ID_SEARCH if notify == EN_CHANGE => on_search_filter_changed(hwnd),
        ID_DEFAULTS => reset_formats(hwnd), // file-type list only (see its tip)
        ID_RESET_ALL => load_defaults(hwnd), // whole dialog → factory defaults
        ID_MENU_RESET => {
            if let Ok(mlist) = GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST) {
                list::reset_menu_order(mlist);
            }
        }
        // The checklist itself lives in a popup editor now — room it never
        // had on the page, and the page gets its breathing space back.
        ID_MENU_ITEMS_EDIT => menuitems::open(hwnd),
        _ => {}
    }
}

/// Affects the currently-shown (filtered) rows; the model
/// syncs via LVN_ITEMCHANGED, so off-screen formats are kept.
pub(super) unsafe fn on_select_clear_all(hwnd: HWND, id: i32) {
    if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
        let on = id == ID_SELECT_ALL;
        let count = SendMessageW(list, LVM_GETITEMCOUNT, None, None).0 as i32;
        for i in 0..count {
            set_check(list, i, on);
        }
    }
}

pub(super) unsafe fn on_search_filter_changed(hwnd: HWND) {
    if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
        let text = get_edit_text(hwnd, ID_SEARCH);
        // EN_CHANGE fires on every keystroke, and populate_list
        // deletes + reinserts all FORMATS rows. Skip that whole rebuild
        // when the NORMALIZED filter hasn't actually changed (a no-op
        // edit, case-only change, or trailing whitespace).
        let needle = text.trim().to_lowercase();
        let changed = LAST_FILTER.with(|f| {
            let mut f = f.borrow_mut();
            if f.as_deref() == Some(needle.as_str()) {
                false
            } else {
                *f = Some(needle);
                true
            }
        });
        if changed {
            populate_list(list, &text);
        }
    }
}

/// The screenshot-tool controls: instant-screenshot / quick-save / custom-action
/// enable toggles, the save-folder picker + toggle, and the restart button.
pub(super) unsafe fn on_command_shot(hwnd: HWND, id: i32) {
    match id {
        // Instant-screenshot checkbox: enable/disable its hotkey picker live —
        // and re-grey its dependent rows (Quick screenshot / save-folder toggle).
        ID_SHOT_ENABLE => {
            refresh_shot_status(hwnd);
            sync_dependent_switches(hwnd);
        }
        ID_SHOT_QUICK_ENABLE => update_quick_enabled(hwnd),
        ID_CUSTOM_ACTION_ENABLE => update_custom_action_enabled(hwnd),
        ID_SHOT_USE_DIR => update_save_dir_enabled(hwnd),
        ID_SHOT_SET_DIR => on_shot_set_dir(hwnd),
        ID_SHOT_RESTART => on_shot_restart(hwnd),
        ID_EDIT_UPLOAD_HOSTS => crate::screenshot::open_hosts_config(),
        _ => {}
    }
}

/// Pick the Ctrl+S save folder; persist immediately + refresh the
/// display. (The toggle next to it is saved with the other settings
/// on the Save button.)
pub(super) unsafe fn on_shot_set_dir(hwnd: HWND) {
    if let Some(dir) = crate::win::pick_folder(hwnd) {
        let _ = settings::set_screenshot_save_dir(&dir);
        set_shot_dir_label(hwnd);
    }
}

/// (Re)start the tray daemon: ensure the autostart entry + a
/// live daemon, then re-register the current hotkey. Tick the
/// Enable box to match, and show an optimistic status (the
/// daemon was just spawned; the true state shows on reopen).
pub(super) unsafe fn on_shot_restart(hwnd: HWND) {
    crate::screenshot::set_enabled(true);
    crate::screenshot::reload_hotkey();
    check(hwnd, ID_SHOT_ENABLE, true);
    set_shot_status(hwnd, t("shot_status_started"), true);
    // check() above is a raw BM_SETCHECK, not a click: it never sends
    // WM_COMMAND, so the normal ID_SHOT_ENABLE handler (which greys/ungreys
    // ID_SHOT_QUICK_ENABLE / ID_SHOT_USE_DIR) never runs on its own here.
    sync_dependent_switches(hwnd);
}

/// Dependent-switch fan-out (menu rows, Quick-preview rows), the sync button, the
/// sign-in nudge card, the language combo, the nav rail, and the sponsor banner.
pub(super) unsafe fn on_command_sync_nav(hwnd: HWND, id: i32, notify: u32) {
    match id {
        // Parent switches with greyed dependents (the menu rows, the Quick-preview
        // rows): one table drives them all — see `DEPENDENT_SWITCHES`.
        ID_ENABLE_MENU | ID_PREVIEW_ENABLED => sync_dependent_switches(hwnd),
        // The badge-style row's parent is a COMBO, not a checkbox, so it arrives as
        // a selection change rather than a click — see `DEPENDENT_ON_COMBO`.
        ID_CORNER_MARK if notify == CBN_SELCHANGE => sync_dependent_switches(hwnd),
        ID_SYNC_BTN => on_sync_click(hwnd),
        ID_NUDGE_ACTION | ID_NUDGE_LATER | ID_NUDGE_MONTH | ID_NUDGE_DISCORD => {
            nudge::on_command(hwnd, id);
        }
        ID_BIZNAG_ACTION | ID_BIZNAG_BUY => {
            biznag::on_command(hwnd, id);
        }
        ID_LANG if notify == CBN_SELCHANGE => on_lang_change(hwnd),
        nav if (ID_NAV_BASE..ID_NAV_BASE + NCAT as i32).contains(&nav) && notify == STN_CLICKED => {
            switch_category(hwnd, (nav - ID_NAV_BASE) as usize);
        }
        ID_BANNER if notify == STN_CLICKED => on_banner_click(hwnd),
        _ => {}
    }
}

/// Open the currently-shown sponsor's link (or the product page if no sponsor
/// feed loaded).
pub(super) unsafe fn on_banner_click(hwnd: HWND) {
    let mut url = None;
    if let Some((_, rot)) = banner_rotator(hwnd) {
        let r = &*rot;
        if let Some(sponsor) = r.sponsors.get(r.cur) {
            url = Some(wstr_to_string(&sponsor.link));
        }
    }
    match url {
        Some(u) if !u.is_empty() => open_url(&u),
        _ => open_url(URL_PRODUCT),
    }
}

/// The admin/diagnostics buttons: About, log, import/export, cache rebuild,
/// association repair, the doctor report, portable registration, update check.
pub(super) unsafe fn on_command_admin(hwnd: HWND, id: i32) {
    match id {
        ID_ABOUT => show_about(hwnd),
        ID_OPEN_LOG => open_diagnostics_log(),
        ID_EXPORT => export_settings_to_file(hwnd),
        ID_IMPORT => import_settings_from_file(hwnd),
        ID_REBUILD_CACHE => rebuild_thumbnail_cache(hwnd),
        ID_REPAIR_ASSOC => repair_associations(hwnd),
        // Owned modal, like the feedback box: Settings stays open behind it.
        ID_RUN_DOCTOR => crate::doctor_report::run_doctor_report(Some(hwnd)),
        ID_PORTABLE_REG => toggle_portable_registration(hwnd),
        ID_CHECK_UPDATES => show_about(hwnd),
        _ => {}
    }
}

/// The Licence page's two network buttons — Redeem and Check now. Both run on a worker
/// thread and post back through `WM_APP_LICENCE`; see `licence_ui.rs`.
pub(super) unsafe fn on_command_licence(hwnd: HWND, id: i32) {
    match id {
        ID_LICENCE_REDEEM_BTN => licence_ui::on_redeem_click(hwnd),
        ID_LICENCE_CHECK_NOW => licence_ui::on_check_now_click(hwnd),
        ID_LICENCE_BUY => crate::win::open_url(crate::license::BUY_URL),
        ID_LICENCE_RENEW => crate::win::open_url(&crate::license::renew_url()),
        ID_LICENCE_MOVE => crate::win::open_url(crate::license::PORTAL_CLAIM_URL),
        _ => {}
    }
}
