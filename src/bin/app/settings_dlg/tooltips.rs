//! The Settings hover-tooltip table: `TOOLTIPS` (control id -> hint locale key, also used
//! by the settings-wide search), and installing/re-translating them on the one shared
//! tooltip window. Split out of `mod.rs`.

use super::*;
use windows::Win32::UI::Controls::TOOLTIP_FLAGS;

/// (control id, hint locale key) for every tooltip. Shared by `add_tooltips`
/// (initial install), `refresh_tooltips` (re-translate on a live language
/// change), and the settings-wide search, which matches against tooltip text so
/// "poster" finds the cover-art switch even though its label says "cover art".
/// The banner's hint is dynamic (rotates with the ad) so it's excluded
/// here and pulled via a TTN_GETDISPINFO callback instead.
pub(super) const TOOLTIPS: &[(i32, &str)] = &[
    (ID_ENABLE_THUMBS, "tip_enable_thumbs"),
    (ID_USE_EMBEDDED, "tip_prefer_embedded"),
    (ID_LBL_CORNER_MARK, "tip_corner_mark"),
    (ID_CORNER_MARK, "tip_corner_mark"),
    (ID_LBL_BADGE_SIZE, "tip_badge_size"),
    (ID_BADGE_SIZE, "tip_badge_size"),
    (ID_BADGE_ICON, "tip_badge_icon"),
    (ID_THUMB_CHECKER, "tip_thumb_checker"),
    (ID_VIDEO_COVER_ART, "tip_video_cover_art"),
    (ID_LBL_VIDEO_OFFSET, "tip_video_offset"),
    (ID_ENABLE_MENU, "tip_enable_menu"),
    (ID_MENU_PREVIEW, "tip_menu_preview"),
    (ID_APP_THEME, "tip_app_theme"),
    (ID_SHOT_TOOL, "tip_shot_tool"),
    (ID_SHOT_DELAY, "tip_shot_delay"),
    (ID_MENU_QUICK, "tip_menu_quick"),
    (ID_MENU_CHECKER, "tip_menu_checker"),
    (ID_FOLDER_PREBUILD, "tip_folder_prebuild"),
    (ID_MAXSIZE, "tip_max_file"),
    (ID_SIZE, "tip_max_thumb"),
    (ID_JPEG, "tip_jpeg"),
    (ID_PNG, "tip_png"),
    // The same hints on the field LABELS (the natural hover target — the edit box is tiny).
    (ID_LBL_MAXFILE, "tip_max_file"),
    (ID_LBL_MAXTHUMB, "tip_max_thumb"),
    (ID_LBL_JPEG, "tip_jpeg"),
    (ID_LBL_PNG, "tip_png"),
    (ID_C_SORT, "tip_sort"),
    (ID_C_PREFER_COVER, "tip_prefer_cover"),
    (ID_C_SKIP_SCAN, "tip_skip_scan"),
    (ID_C_ARCHIVE_SHEET, "tip_archive_sheet"),
    (ID_LANG, "tip_lang"),
    (ID_SHOT_ENABLE, "tip_screenshot"),
    (ID_SHOT_HOTKEY, "tip_shot_hotkey"),
    (ID_SHOT_QUICK_ENABLE, "tip_instant_screenshot"),
    (ID_SHOT_QUICK_HOTKEY, "tip_shot_quick_hotkey"),
    (ID_SHOT_USE_DIR, "tip_shot_use_dir"),
    (ID_SHOT_SET_DIR, "tip_shot_set_dir"),
    (ID_EDIT_UPLOAD_HOSTS, "tip_edit_upload_hosts"),
    (ID_SHOT_RESTART, "tip_shot_restart"),
    (ID_SHOT_HIDE_TRAY, "tip_hide_tray"),
    (ID_CUSTOM_ACTION_ENABLE, "tip_custom_action_enable"),
    (ID_PREVIEW_ENABLED, "tip_preview_enabled"),
    (ID_PREVIEW_HOLD_PEEK, "tip_preview_hold_peek"),
    (ID_PREVIEW_CLOSE_FOCUS, "tip_preview_close_focus"),
    (ID_PREVIEW_TOPMOST, "tip_preview_topmost"),
    (ID_PREVIEW_TEXT, "tip_preview_text"),
    (ID_PREVIEW_MARKDOWN, "tip_preview_markdown"),
    (ID_SHOT_ACTION, "tip_custom_action"),
    (ID_SHOT_ACTION_HK, "tip_custom_action_hk"),
    // Same hints on the combo LABELS, like the Limits fields above.
    (ID_LBL_SHOT_ACTION, "tip_custom_action"),
    (ID_LBL_SHOT_ACTION_HK, "tip_custom_action_hk"),
    (ID_MENU_ITEMS_LIST, "tip_menu_items"),
    (ID_MENU_RESET, "tip_menu_reset"),
    (ID_VERBOSE_LOG, "tip_verbose_log"),
    (ID_OPEN_LOG, "tip_open_log"),
    (ID_REBUILD_CACHE, "tip_rebuild_cache"),
    (ID_REPAIR_ASSOC, "tip_repair_assoc"),
    (ID_RUN_DOCTOR, "tip_run_doctor"),
    (ID_PORTABLE_REG, "tip_portable_register"),
    (ID_UPDATE_AUTO, "tip_update_auto"),
    (ID_CHECK_UPDATES, "tip_check_updates"),
    (ID_RESET_ALL, "tip_reset_all"),
    (ID_IMPORT, "tip_import"),
    (ID_EXPORT, "tip_export"),
    (ID_LICENCE_KEY_EDIT, "tip_licence_key"),
    (ID_LICENCE_REDEEM_BTN, "tip_licence_redeem"),
    (ID_LICENCE_CHECK_NOW, "tip_licence_check_now"),
    (ID_LICENCE_RENEW, "tip_licence_renew"),
    (ID_LICENCE_BUY, "tip_licence_buy"),
    (ID_LICENCE_MOVE, "tip_licence_move"),
    (ID_SELECT_ALL, "tip_select_all"),
    (ID_CLEAR_ALL, "tip_clear_all"),
    (ID_DEFAULTS, "tip_defaults"),
    (ID_LIST, "tip_list"),
    (ID_ABOUT, "tip_about"),
    (IDOK, "tip_save"),
    (IDCANCEL, "tip_cancel"),
];
/// Edit-text message for the comctl32 tooltip (not in this windows-rs metadata).
const TTM_UPDATETIPTEXTW: u32 = WM_USER + 57;

/// Build the `TTTOOLINFOW` that names `ctl`'s hint, with `lpszText` pointing at
/// the caller-owned `text` buffer, which must outlive the `SendMessageW` that
/// consumes the struct.
fn tool_info(hwnd: HWND, ctl: HWND, text: &[u16], u_flags: TOOLTIP_FLAGS) -> TTTOOLINFOW {
    TTTOOLINFOW {
        cbSize: core::mem::size_of::<TTTOOLINFOW>() as u32,
        uFlags: u_flags,
        hwnd,
        uId: ctl.0 as usize,
        lpszText: PWSTR(text.as_ptr() as *mut u16),
        ..Default::default()
    }
}

/// Attach a hover hint to every interactive Settings control. One tooltip window
/// owns them all; `TTF_SUBCLASS` lets it relay its own mouse messages, so the
/// dialog's wndproc needs no extra handling. Hint text is localized with an
/// English fallback, so untranslated locales still get a hint. Labels stay plain
/// STATICs (no SS_NOTIFY = no mouse messages), so the hint rides the control they
/// describe — which is what a user actually hovers. The tooltip window HWND is
/// stashed in the dialog's GWLP_USERDATA so `refresh_tooltips` can re-text it.
pub(super) unsafe fn add_tooltips(hwnd: HWND, hinst: HINSTANCE) {
    let Ok(tip) = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("tooltips_class32"),
        PCWSTR::null(),
        WS_POPUP | WINDOW_STYLE(TTS_ALWAYSTIP | TTS_NOPREFIX),
        0,
        0,
        0,
        0,
        Some(hwnd),
        None,
        Some(hinst),
        None,
    ) else {
        return;
    };
    // Let long hints wrap (and honor explicit line breaks) instead of one wide line.
    SendMessageW(tip, TTM_SETMAXTIPWIDTH, Some(WPARAM(0)), Some(LPARAM(320)));
    // Remember the tooltip window so a live language change can re-text it.
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, tip.0 as isize);

    // The fixed-text controls.
    for &(id, key) in TOOLTIPS {
        let Ok(ctl) = GetDlgItem(Some(hwnd), id) else {
            continue;
        };
        // comctl32 copies the text on TTM_ADDTOOL, so this buffer can be temporary.
        let text = wide(t(key));
        let mut ti = tool_info(hwnd, ctl, &text, TTF_IDISHWND | TTF_SUBCLASS);
        SendMessageW(
            tip,
            TTM_ADDTOOLW,
            Some(WPARAM(0)),
            Some(LPARAM(&mut ti as *mut _ as isize)),
        );
    }
    // The banner's hint rotates with the ad, so it pulls live text via a
    // TTN_GETDISPINFO callback (handled in WM_NOTIFY) instead of fixed text.
    if let Ok(banner) = GetDlgItem(Some(hwnd), ID_BANNER) {
        let mut ti = TTTOOLINFOW {
            cbSize: core::mem::size_of::<TTTOOLINFOW>() as u32,
            uFlags: TTF_IDISHWND | TTF_SUBCLASS,
            hwnd,
            uId: banner.0 as usize,
            lpszText: PWSTR((-1isize) as *mut u16), // LPSTR_TEXTCALLBACKW
            ..Default::default()
        };
        SendMessageW(
            tip,
            TTM_ADDTOOLW,
            Some(WPARAM(0)),
            Some(LPARAM(&mut ti as *mut _ as isize)),
        );
    }
}

/// Re-text every fixed tooltip in the active language (after a live language
/// switch). The banner's callback-driven hint refreshes itself on the next hover,
/// so it's left alone. No-op if the tooltip window wasn't created.
pub(super) unsafe fn refresh_tooltips(hwnd: HWND) {
    let tip = HWND(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut c_void);
    if tip.is_invalid() {
        return;
    }
    for &(id, key) in TOOLTIPS {
        let Ok(ctl) = GetDlgItem(Some(hwnd), id) else {
            continue;
        };
        let text = wide(t(key));
        let mut ti = tool_info(hwnd, ctl, &text, TTF_IDISHWND);
        SendMessageW(
            tip,
            TTM_UPDATETIPTEXTW,
            Some(WPARAM(0)),
            Some(LPARAM(&mut ti as *mut _ as isize)),
        );
    }
}
