//! Localization + live relabeling (extracted from settings_dlg; parent-hub pattern).

use super::*;

// ---- Localization helpers ----------------------------------------------

/// All shipped language codes (English first).
pub(super) fn lang_codes() -> Vec<&'static str> {
    i18n::codes().collect()
}

/// Fill the language combo: item 0 = "follow system", then each language by its
/// native name. Selects the current override (or "system" if none).
pub(super) unsafe fn fill_lang_combo(combo: HWND) {
    add_combo_string(combo, t("lang_system"));
    let current = settings::lang_override();
    let mut sel = 0i32;
    for (i, code) in lang_codes().iter().enumerate() {
        add_combo_string(combo, i18n::native_name(code));
        if current.as_deref() == Some(*code) {
            sel = (i + 1) as i32;
        }
    }
    SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(sel as usize)), None);
}

pub(super) unsafe fn add_combo_string(combo: HWND, s: &str) {
    let w = wide(s);
    SendMessageW(combo, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
}

/// The language code selected in the combo, or None for "follow system".
pub(super) unsafe fn selected_lang(hwnd: HWND) -> Option<&'static str> {
    let combo = GetDlgItem(Some(hwnd), ID_LANG).ok()?;
    let sel = SendMessageW(combo, CB_GETCURSEL, None, None).0;
    if sel <= 0 {
        None
    } else {
        lang_codes().get((sel - 1) as usize).copied()
    }
}

/// Live language preview: re-resolve the locale and re-label every control
/// (without persisting — persistence happens on OK).
pub(super) unsafe fn on_lang_change(hwnd: HWND) {
    i18n::apply_override_or_system(selected_lang(hwnd));
    apply_labels(hwnd);
    // The search cache keys on the (now stale-language) needle; clear it so the next
    // EN_CHANGE re-filters instead of short-circuiting on an identical needle.
    LAST_FILTER.with(|f| *f.borrow_mut() = None);
}

/// Re-apply every translatable label in the active language (used after a live
/// language change). Edits/selections are preserved (we only set text).
pub(super) unsafe fn apply_labels(hwnd: HWND) {
    set_window_title(hwnd);
    let pairs: &[(i32, &str)] = &[
        (ID_LBL_THUMBS, "grp_thumbnails"),
        (ID_ENABLE_THUMBS, "chk_enable_thumbs"),
        (ID_USE_EMBEDDED, "chk_prefer_embedded"),
        (ID_ENABLE_MENU, "chk_enable_menu"),
        (ID_LBL_PREVIEW, "lbl_menu_preview"),
        (ID_MENU_QUICK, "chk_menu_quick"),
        (ID_MENU_CHECKER, "chk_menu_checker"),
        (ID_LBL_LIMITS, "grp_limits"),
        (ID_LBL_MAXFILE, "lbl_max_file"),
        (ID_LBL_MAXTHUMB, "lbl_max_thumb"),
        (ID_LBL_JPEG, "lbl_jpeg"),
        (ID_LBL_PNG, "lbl_png"),
        (ID_LBL_EBOOK, "grp_ebook"),
        (ID_LBL_GENERAL, "grp_lang_files"),
        (ID_C_SORT, "chk_sort"),
        (ID_C_PREFER_COVER, "chk_prefer_cover"),
        (ID_C_SKIP_SCAN, "chk_skip_scanlation"),
        (ID_C_ARCHIVE_SHEET, "chk_archive_sheet"),
        (ID_LBL_FORMATS, "lbl_formats"),
        (ID_SELECT_ALL, "btn_select_all"),
        (ID_CLEAR_ALL, "btn_clear_all"),
        (ID_DEFAULTS, "btn_defaults"),
        (ID_LBL_LANG, "lbl_language"),
        (ID_LBL_SHOT, "grp_screenshots"),
        (ID_SHOT_ENABLE, "chk_screenshot"),
        (ID_SHOT_HIDE_TRAY, "chk_hide_tray"),
        (ID_LBL_SHOT_HK, "lbl_shot_hotkey"),
        (ID_SHOT_QUICK_ENABLE, "chk_instant_screenshot"),
        (ID_LBL_SHOT_QUICK_HK, "lbl_shot_quick_hotkey"),
        (ID_PRESERVE_DATE, "chk_preserve_date"),
        (ID_LBL_MENU_ITEMS, "grp_menu_items"),
        (ID_MENU_ALL_TYPES, "chk_menu_all_types"),
        (ID_MENU_RESET, "btn_menu_reset"),
        (ID_SHOT_USE_DIR, "chk_shot_use_dir"),
        (ID_SHOT_SET_DIR, "btn_set_save_dir"),
        (ID_SHOT_RESTART, "btn_restart_hotkey"),
        (ID_LBL_SHOT_ACTION, "lbl_custom_action"),
        (ID_LBL_SHOT_ACTION_HK, "lbl_custom_action_hk"),
        (ID_CUSTOM_ACTION_ENABLE, "chk_custom_action"),
        (ID_PREVIEW_ENABLED, "chk_preview_enabled"),
        (ID_PREVIEW_HOLD_PEEK, "chk_preview_hold_peek"),
        (ID_PREVIEW_CLOSE_FOCUS, "chk_preview_close_focus"),
        (ID_PREVIEW_TOPMOST, "chk_preview_topmost"),
        (ID_PREVIEW_TEXT, "chk_preview_text"),
        (ID_PREVIEW_MARKDOWN, "chk_preview_markdown"),
        (ID_LBL_DIAG, "grp_diagnostics"),
        (ID_VERBOSE_LOG, "chk_verbose_log"),
        (ID_OPEN_LOG, "btn_open_log"),
        (ID_REBUILD_CACHE, "btn_rebuild_cache"),
        (ID_REPAIR_ASSOC, "btn_repair_assoc"),
        (ID_UPDATE_AUTO, "chk_update_auto"),
        (ID_CHECK_UPDATES, "btn_check_updates"),
        (ID_LBL_UPDATES, "grp_updates"),
        (ID_LBL_BACKUP, "grp_backup"),
        (ID_LBL_HOTKEY_SVC, "grp_hotkey_svc"),
        (ID_RESET_ALL, "btn_reset_all"),
        (ID_IMPORT, "btn_import"),
        (ID_EXPORT, "btn_export"),
        (IDOK, "btn_ok"),
        (IDCANCEL, "btn_cancel"),
    ];
    for &(id, key) in pairs {
        set_dlg_text(hwnd, id, t(key));
    }
    // Re-text + repaint the owner-draw nav rail and the page header (they read their
    // labels from nav_label()/cat_blurb(), which now follow the active language).
    for i in 0..NCAT as i32 {
        set_dlg_text(hwnd, ID_NAV_BASE + i, nav_label(i as usize));
        if let Ok(nav) = GetDlgItem(Some(hwnd), ID_NAV_BASE + i) {
            let _ = InvalidateRect(Some(nav), None, true);
        }
    }
    if let Ok(ph) = GetDlgItem(Some(hwnd), ID_PANE_HEADER) {
        let _ = InvalidateRect(Some(ph), None, true);
    }
    // The "Menu items" checklist rows relabel from their own menu keys (single col).
    // Rows may be in a custom drag-reorder, so read each ROW's key from its lParam —
    // relabeling by fixed toggle index would scramble the labels after a reorder.
    if let Ok(mlist) = GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST) {
        let count = SendMessageW(mlist, LVM_GETITEMCOUNT, None, None).0 as i32;
        for row in 0..count {
            if let Some(ti) = menu_row_toggle(mlist, row) {
                set_subitem(mlist, row, 0, t(MENU_ITEM_TOGGLES[ti].1));
            }
        }
    }
    if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
        // Columns are Extension | Category | Description (matching build_controls).
        // The old code relabeled column 1 with the *description* header (wrong index)
        // and never touched column 2, so a live language switch left "Category"
        // English and "Description" stale — fixed: correct indices + all three.
        set_column_text(list, 0, t("col_extension"));
        set_column_text(list, 1, t("col_category"));
        set_column_text(list, 2, t("col_description"));
    }
    // The preview-placement combo holds translated items: rebuild, keep selection.
    if let Ok(prev) = GetDlgItem(Some(hwnd), ID_MENU_PREVIEW) {
        let sel = SendMessageW(prev, CB_GETCURSEL, None, None).0.max(0);
        SendMessageW(prev, CB_RESETCONTENT, None, None);
        for key in ["prev_off", "prev_submenu", "prev_main"] {
            let w = wide(t(key));
            SendMessageW(prev, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
        }
        SendMessageW(prev, CB_SETCURSEL, Some(WPARAM(sel as usize)), None);
    }
    // The hover hints were also baked in the old language — re-text them.
    refresh_tooltips(hwnd);
}

pub(super) unsafe fn set_dlg_text(hwnd: HWND, id: i32, s: &str) {
    if let Ok(h) = GetDlgItem(Some(hwnd), id) {
        let w = wide(s);
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
    }
}

pub(super) unsafe fn set_window_title(hwnd: HWND) {
    let title = format!("SageThumbs 2K \u{2014} {}", t("lbl_options"));
    let w = wide(&title);
    let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr()));
}

pub(super) unsafe fn set_column_text(list: HWND, idx: i32, s: &str) {
    let w = wide(s);
    let mut col = LVCOLUMNW {
        mask: LVCF_TEXT,
        pszText: PWSTR(w.as_ptr() as *mut u16),
        ..Default::default()
    };
    SendMessageW(list, LVM_SETCOLUMNW, Some(WPARAM(idx as usize)), Some(LPARAM(&mut col as *mut _ as isize)));
}

/// Repeating timer that keeps the hotkey-service status line live while Settings is open, so
/// it reflects a self-heal / watchdog restart (or the daemon dying) without reopening the
/// dialog. IDs 1–2 are the sponsor banner timers (see [`crate::sponsors`]); this is the third.
pub(super) const TIMER_SHOT_STATUS: usize = 3;
