//! The left column, one section per settings page: thumbnails, general, menu items, screenshots, diagnostics, sync and quick preview.

use super::*;

/// Left column: options — one vertical rhythm via the LeftCol cursor
pub(super) unsafe fn build_thumbnails(hwnd: HWND, lc: &mut LeftCol, sty: &Styles) {
    lc.header(t("grp_thumbnails"), sty.hdr, ID_LBL_THUMBS, true);
    // Portable build only: the per-user Explorer registration.
    //
    // Gated on the SAME condition as its `cat_rows` row, and that is not optional. Creating
    // these unconditionally and letting the missing row hide them does NOT work: a control the
    // layout never visits keeps the position it was created at, so on an installed build the
    // button and its status floated over the nav rail and the page header. Caught by shooting
    // the page; nothing about the code reads wrong.
    if sagethumbs2k_core::settings::portable() {
        lc.status(ID_PORTABLE_REG_STATUS);
        if let Ok(h) = GetDlgItem(Some(hwnd), ID_PORTABLE_REG_STATUS) {
            const SS_RIGHT: u32 = 0x0002;
            let st = GetWindowLongW(h, GWL_STYLE) as u32 | SS_RIGHT;
            SetWindowLongW(h, GWL_STYLE, st as i32);
        }
        lc.button(t("btn_portable_register"), 240, ID_PORTABLE_REG);
    }
    lc.checkbox(t("chk_enable_thumbs"), sty.cb, 300, ID_ENABLE_THUMBS);
    lc.checkbox(t("chk_prefer_embedded"), sty.cb, 300, ID_USE_EMBEDDED);
    lc.checkbox(t("chk_badge_icon"), sty.cb, 300, ID_BADGE_ICON);
    lc.checkbox(t("chk_thumb_checker"), sty.cb, 300, ID_THUMB_CHECKER);
    lc.checkbox(t("chk_video_cover_art"), sty.cb, 300, ID_VIDEO_COVER_ART);
    // Headers the v3 layout places, not this legacy column: File types' two sections,
    // the menu page's verb-behavior split, and Quick preview's content-type split.
    lc.header(t("grp_tile_look"), sty.hdr, ID_LBL_TILE_LOOK, false);
    lc.header(t("grp_formats_pick"), sty.hdr, ID_LBL_FORMATS_PICK, false);
    lc.header(t("grp_convert_verbs"), sty.hdr, ID_LBL_CONVERT_VERBS, false);
    lc.header(t("grp_preview_kinds"), sty.hdr, ID_LBL_PREVIEW_KINDS, false);
    lc.header(t("grp_menu_look"), sty.hdr, ID_LBL_MENU_LOOK, false);
    lc.header(t("grp_quickaction"), sty.hdr, ID_LBL_QUICKACTION, false);
    lc.header(
        t("grp_preview_behavior"),
        sty.hdr,
        ID_LBL_PREVIEW_BEHAVIOR,
        false,
    );
    lc.button(t("btn_menu_items_edit"), 200, ID_MENU_ITEMS_EDIT);

    // Limits & quality — numeric label+edit rows. Single-line edits top-align +
    // ignore EM_SETRECT, so they're kept snug; the rounded field panel behind them
    // (biased up) supplies the box height and centers the digits.
    lc.header(t("grp_limits"), sty.hdr, ID_LBL_LIMITS, false);
    lc.edit(
        t("lbl_max_file"),
        ID_LBL_MAXFILE,
        sty.edit_style,
        ID_MAXSIZE,
    );
    lc.edit(t("lbl_max_thumb"), ID_LBL_MAXTHUMB, sty.edit_style, ID_SIZE);
    lc.edit(t("lbl_jpeg"), ID_LBL_JPEG, sty.edit_style, ID_JPEG);
    lc.edit(t("lbl_png"), ID_LBL_PNG, sty.edit_style, ID_PNG);
    // Created with the other numeric rows; `navrail::cat_rows` puts it on Appearance, next to
    // the video cover-art switch it belongs with (creation order carries no meaning here).
    lc.edit(
        t("lbl_video_offset"),
        ID_LBL_VIDEO_OFFSET,
        sty.edit_style,
        ID_VIDEO_OFFSET,
    );

    // Ebook & comic archive cover options (the DarkThumbs toggles).
    lc.header(t("grp_ebook"), sty.hdr, ID_LBL_EBOOK, false);
    lc.checkbox(t("chk_sort"), sty.cb, 312, ID_C_SORT);
    lc.checkbox(t("chk_prefer_cover"), sty.cb, 312, ID_C_PREFER_COVER);
    lc.checkbox(t("chk_skip_scanlation"), sty.cb, 312, ID_C_SKIP_SCAN);
    lc.checkbox(t("chk_archive_sheet"), sty.cb, 312, ID_C_ARCHIVE_SHEET);
}

/// General: right-click menu integration + UI language
pub(super) unsafe fn build_general(lc: &mut LeftCol, sty: &Styles) {
    // Menu toggles grouped as checkboxes, then the two dropdowns below them.
    lc.header(t("grp_lang_files"), sty.hdr, ID_LBL_GENERAL, false);
    lc.checkbox(t("chk_enable_menu"), sty.cb, 300, ID_ENABLE_MENU);
    lc.checkbox(t("chk_menu_all_types"), sty.cb, 300, ID_MENU_ALL_TYPES);
    lc.checkbox(t("chk_menu_quick"), sty.cb, 312, ID_MENU_QUICK);
    lc.checkbox(t("chk_menu_checker"), sty.cb, 300, ID_MENU_CHECKER);
    lc.checkbox(t("chk_folder_prebuild"), sty.cb, 312, ID_FOLDER_PREBUILD);
    lc.checkbox(t("chk_preserve_date"), sty.cb, 312, ID_PRESERVE_DATE);
    lc.checkbox(t("chk_keep_metadata"), sty.cb, 312, ID_KEEP_METADATA);
    lc.checkbox(t("chk_pdf_margin"), sty.cb, 312, ID_PDF_MARGIN);
    let prev = lc.combo(t("lbl_menu_preview"), ID_LBL_PREVIEW, 160, ID_MENU_PREVIEW);
    for key in ["prev_off", "prev_submenu", "prev_main"] {
        let w = wide(t(key));
        SendMessageW(prev, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
    }
    SendMessageW(
        prev,
        CB_SETCURSEL,
        Some(WPARAM(settings::menu_preview() as usize)),
        None,
    );
    // Widen the dropdown beyond the closed box so longer option labels (and longer
    // translations) aren't clipped.
    SendMessageW(prev, CB_SETDROPPEDWIDTH, Some(WPARAM(230)), None);
    dark_theme_combo(prev);
    restyle::dark_combo_subclass(prev, ID_MENU_PREVIEW);

    // The corner of the tile: Explorer's own type icon, our format mark, or nothing. One
    // three-way choice, because those three are mutually exclusive answers to one question and
    // the two checkboxes it replaced could be set to a combination that produced neither
    // (see `settings::CornerMark`). Option order IS the stored value — `CornerMark::as_dword`.
    // Created at the width `navrail::cat_rows` also lays it out at, so the two agree if anyone
    // reads only one of them; the layout is what actually wins.
    let corner = lc.combo(
        t("lbl_corner_mark"),
        ID_LBL_CORNER_MARK,
        216,
        ID_CORNER_MARK,
    );
    for key in [
        "corner_mark_system",
        "corner_mark_badge",
        "corner_mark_none",
    ] {
        let w = wide(t(key));
        SendMessageW(
            corner,
            CB_ADDSTRING,
            None,
            Some(LPARAM(w.as_ptr() as isize)),
        );
    }
    SendMessageW(
        corner,
        CB_SETCURSEL,
        Some(WPARAM(settings::corner_mark().as_dword() as usize)),
        None,
    );
    SendMessageW(corner, CB_SETDROPPEDWIDTH, Some(WPARAM(280)), None);
    dark_theme_combo(corner);
    restyle::dark_combo_subclass(corner, ID_CORNER_MARK);

    // How big that mark is drawn. Same shape as the combo above and laid out right under it:
    // option order IS the stored value (`BadgeSize::as_dword`). Narrower than the corner combo
    // because its options are one word each.
    let badge_size = lc.combo(t("lbl_badge_size"), ID_LBL_BADGE_SIZE, 156, ID_BADGE_SIZE);
    for key in ["badge_size_small", "badge_size_medium", "badge_size_large"] {
        let w = wide(t(key));
        SendMessageW(
            badge_size,
            CB_ADDSTRING,
            None,
            Some(LPARAM(w.as_ptr() as isize)),
        );
    }
    SendMessageW(
        badge_size,
        CB_SETCURSEL,
        Some(WPARAM(settings::badge_size().as_dword() as usize)),
        None,
    );
    SendMessageW(badge_size, CB_SETDROPPEDWIDTH, Some(WPARAM(230)), None);
    dark_theme_combo(badge_size);
    restyle::dark_combo_subclass(badge_size, ID_BADGE_SIZE);

    let theme = lc.combo(t("lbl_app_theme"), ID_LBL_APP_THEME, 160, ID_APP_THEME);
    for key in ["theme_system", "theme_light", "theme_dark"] {
        let w = wide(t(key));
        SendMessageW(theme, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
    }
    SendMessageW(
        theme,
        CB_SETCURSEL,
        Some(WPARAM(settings::app_theme() as usize)),
        None,
    );
    SendMessageW(theme, CB_SETDROPPEDWIDTH, Some(WPARAM(230)), None);
    dark_theme_combo(theme);
    restyle::dark_combo_subclass(theme, ID_APP_THEME);

    let shot_tool = lc.combo(t("lbl_shot_tool"), ID_LBL_SHOT_TOOL, 160, ID_SHOT_TOOL);
    // Option order comes from Tool::DEFAULTABLE, so the dropdown and the stored index can
    // never drift apart: the array IS the wire format.
    for key in [
        "tool_arrow",
        "tool_rect",
        "tool_ellipse",
        "tool_line",
        "tool_pen",
        "tool_text",
        "tool_number",
        "tool_highlight",
        "tool_pixelate",
        "tool_invert",
    ] {
        let w = wide(t(key));
        SendMessageW(
            shot_tool,
            CB_ADDSTRING,
            None,
            Some(LPARAM(w.as_ptr() as isize)),
        );
    }
    // Out-of-range FALLS BACK, it does not clamp. A hand-edited registry value used to select
    // nothing at all and render the combo BLANK; clamping to the last entry was no better,
    // because it then showed "Invert" while the capture editor was actually starting in Arrow.
    // `Tool::from_default_index` degrades to the default, so this has to degrade identically
    // or the dialog reports a tool the editor is not using.
    let raw_tool = settings::screenshot_default_tool();
    let tool_sel = if raw_tool < settings::SHOT_TOOL_COUNT {
        raw_tool
    } else {
        settings::DEFAULT_SHOT_TOOL
    };
    SendMessageW(
        shot_tool,
        CB_SETCURSEL,
        Some(WPARAM(tool_sel as usize)),
        None,
    );
    SendMessageW(shot_tool, CB_SETDROPPEDWIDTH, Some(WPARAM(230)), None);
    dark_theme_combo(shot_tool);
    restyle::dark_combo_subclass(shot_tool, ID_SHOT_TOOL);

    // Delay before a capture freezes the screen. Option order comes from
    // settings::SHOT_DELAY_STEPS — the array is the wire format, so the dropdown and the
    // stored seconds cannot drift apart.
    let delay = lc.combo(t("lbl_shot_delay"), ID_LBL_SHOT_DELAY, 160, ID_SHOT_DELAY);
    for key in ["delay_off", "delay_1", "delay_2", "delay_3", "delay_5"] {
        let w = wide(t(key));
        SendMessageW(delay, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
    }
    SendMessageW(
        delay,
        CB_SETCURSEL,
        Some(WPARAM(
            shot_delay_combo_index(settings::screenshot_delay_sec()) as usize,
        )),
        None,
    );
    SendMessageW(delay, CB_SETDROPPEDWIDTH, Some(WPARAM(230)), None);
    dark_theme_combo(delay);
    restyle::dark_combo_subclass(delay, ID_SHOT_DELAY);

    let combo = lc.combo(t("lbl_language"), ID_LBL_LANG, 260, ID_LANG);
    fill_lang_combo(combo);
    // The closed box is narrow, but the dropdown is wider so long native language
    // names aren't truncated in the list.
    SendMessageW(combo, CB_SETDROPPEDWIDTH, Some(WPARAM(220)), None);
    dark_theme_combo(combo);
    restyle::dark_combo_subclass(combo, ID_LANG);
}

/// Menu items: show/hide each SageThumbs 2K context-menu entry
pub(super) unsafe fn build_menu_items(hwnd: HWND, lc: &mut LeftCol, sty: &Styles) {
    // XnShell-style "Displayed menu items" checklist; each label reuses the menu
    // item's own translated name. (Settings is always shown, so it isn't listed.)
    lc.header(t("grp_menu_items"), sty.hdr, ID_LBL_MENU_ITEMS, false);
    // The checklist is sized to fit EXACTLY its rows (measured below) — no inner
    // scrollbar, no slack/gap. Wheeling over it scrolls the OUTER column (wheel-forward
    // subclass), so a nested scroll would strand the bottom rows.
    let list_y_before = lc.y;
    let mlist = lc.checklist(20, ID_MENU_ITEMS_LIST); // provisional; exact-fit resize below
    insert_column(mlist, 0, "", 300); // single full-width column, no header title
                                      // Seed the rows in the saved DISPLAY order: item rows (tagged with their toggle index
                                      // in lParam) interleaved with divider rows (tagged `list::SEP_PARAM`), so a
                                      // drag-reorder of either round-trips on save. Falls back to the factory order.
    let rows = saved_menu_rows();
    list::rebuild_rows(mlist, &rows, None);
    // Exact-fit: resize the list to its REAL measured report-row height × N rows
    // (font/DPI-proof — no estimate, no clip, no bottom gap), then re-anchor the cursor
    // to the list's true bottom so the sections below sit right under it.
    {
        let mut r = RECT::default(); // .left = LVIR_BOUNDS (0)
        SendMessageW(
            mlist,
            windows::Win32::UI::Controls::LVM_GETITEMRECT,
            Some(WPARAM(0)),
            Some(LPARAM(&mut r as *mut RECT as isize)),
        );
        let row_dev = (r.bottom - r.top).max(1);
        let needed_dev = rows.len() as i32 * row_dev + 2; // +2px guards a rounding scrollbar
        let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96) as i32;
        let _ = SetWindowPos(
            mlist,
            None,
            0,
            0,
            dpi_scale(hwnd, 322),
            needed_dev,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        lc.y = list_y_before + MT_CHECK + needed_dev * 96 / dpi;
    }
    // A subtle "Reset order" button under the list — restores the default drag order
    // when a reorder gets messy (keeps each item's checkbox state).
    lc.button(t("btn_menu_reset"), 110, ID_MENU_RESET);
    // Check states are seeded in load_values (rows exist now).
}

/// Screenshots: capture service + hotkey
pub(super) unsafe fn build_screenshots(hwnd: HWND, lc: &mut LeftCol, sty: &Styles) {
    // The opt-in screen-capture controls (enable toggle + hotkey preset). The enable
    // checkbox seeds in load_values; the picker seeds inline from the stored hotkey.
    lc.header(t("grp_screenshots"), sty.hdr, ID_LBL_SHOT, false);
    lc.checkbox(t("chk_screenshot"), sty.cb, 300, ID_SHOT_ENABLE);
    // Owner layout pref: group the screenshot CHECKBOXES together, then the hotkey
    // DROPDOWNS together below. The instant-screenshot checkbox gates the Quick-save
    // combo further down (that combo greys out while this is unchecked).
    lc.checkbox(t("chk_hide_tray"), sty.cb, 300, ID_SHOT_HIDE_TRAY);
    lc.checkbox(
        t("chk_instant_screenshot"),
        sty.cb,
        300,
        ID_SHOT_QUICK_ENABLE,
    );
    // Ctrl+S destination toggle — kept WITH the other screenshot checkboxes (owner pref:
    // checkboxes grouped, then dropdowns). On → auto-save to the fixed folder below
    // (Desktop by default); off → Ctrl+S prompts each time. (Ctrl+C always copies.)
    lc.checkbox(t("chk_shot_use_dir"), sty.cb, 300, ID_SHOT_USE_DIR);
    let shot = lc.combo(t("lbl_shot_hotkey"), ID_LBL_SHOT_HK, 200, ID_SHOT_HOTKEY);
    // Select the preset matching the stored hotkey (default = first = Ctrl+PrtScn).
    // A legacy/foreign chord (not in the curated list) gets its own trailing item
    // instead of collapsing to the default — see `populate_hotkey_presets`.
    let (m, v) = settings::screenshot_hotkey();
    let packed = (m << 8) | v;
    let sel = populate_hotkey_presets(shot, packed, 0);
    SendMessageW(shot, CB_SETCURSEL, Some(WPARAM(sel)), None);
    dark_theme_combo(shot);
    restyle::dark_combo_subclass(shot, ID_SHOT_HOTKEY);
    // Quick-save hotkey picker — grouped directly under the capture-hotkey combo.
    // Gated by the "instant screenshot" checkbox above (see `update_quick_enabled`);
    // greyed out while that box is unchecked.
    let quick = lc.combo(
        t("lbl_shot_quick_hotkey"),
        ID_LBL_SHOT_QUICK_HK,
        200,
        ID_SHOT_QUICK_HOTKEY,
    );
    // Select the saved chord, or default to one that won't collide with the main
    // Ctrl+PrtScn, so flipping the checkbox on just works. 0 = genuinely unset
    // (falls to the quick default); any other unrecognized value is a real
    // stored chord and gets its own trailing item.
    let (qm, qv) = settings::screenshot_quick_hotkey();
    let qpacked = (qm << 8) | qv;
    let quick_default = SHOT_PRESETS
        .iter()
        .position(|&(l, _)| l == QUICK_DEFAULT_LABEL)
        .unwrap_or(0);
    let qsel = populate_hotkey_presets(quick, qpacked, quick_default);
    SendMessageW(quick, CB_SETCURSEL, Some(WPARAM(qsel)), None);
    dark_theme_combo(quick);
    restyle::dark_combo_subclass(quick, ID_SHOT_QUICK_HOTKEY);
    // Custom action hotkey: ONE user-assignable [action] + [hotkey] binding (the owner's
    // "two dropdowns" request). The chosen action fires from a global hotkey owned by this
    // same daemon. The action combo lists the curated `hotkey::ACTIONS`; the hotkey combo is
    // a "(none)" entry + the SHOT_PRESETS chords, where "(none)" = unbound. Seeded inline
    // from settings; persisted in apply_settings; reset in load_defaults.
    let act = lc.combo(
        t("lbl_custom_action"),
        ID_LBL_SHOT_ACTION,
        200,
        ID_SHOT_ACTION,
    );
    for &(_, key) in crate::hotkey::ACTIONS {
        let w = wide(crate::hotkey::action_label(key));
        SendMessageW(act, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
    }
    let cur_action = settings::custom_action();
    let asel = crate::hotkey::ACTIONS
        .iter()
        .position(|&(id, _)| id == cur_action)
        .unwrap_or(0);
    SendMessageW(act, CB_SETCURSEL, Some(WPARAM(asel)), None);
    dark_theme_combo(act);
    restyle::dark_combo_subclass(act, ID_SHOT_ACTION);
    // Its hotkey: item 0 is "(none)" (unbound); items 1.. mirror SHOT_PRESETS.
    let ahk = lc.combo(
        t("lbl_custom_action_hk"),
        ID_LBL_SHOT_ACTION_HK,
        220,
        ID_SHOT_ACTION_HK,
    );
    let none_w = wide(t("opt_none_unassigned"));
    let none_idx = SendMessageW(
        ahk,
        CB_ADDSTRING,
        None,
        Some(LPARAM(none_w.as_ptr() as isize)),
    )
    .0;
    // Item data mirrors the packed chord (0 = unbound) for every item, "(none)"
    // included, so Save reads it back with CB_GETITEMDATA instead of re-deriving
    // it from position (see `values::apply_settings`).
    SendMessageW(
        ahk,
        CB_SETITEMDATA,
        Some(WPARAM(none_idx as usize)),
        Some(LPARAM(0)),
    );
    append_shot_presets(ahk);
    let (cam, cav) = settings::custom_action_hotkey();
    let cpacked = (cam << 8) | cav;
    let hksel = if cav == 0 {
        0
    } else {
        SHOT_PRESETS
            .iter()
            .position(|&(_, p)| p == cpacked)
            .map_or_else(|| append_unknown_chord_item(ahk, cpacked), |i| i + 1)
    };
    SendMessageW(ahk, CB_SETCURSEL, Some(WPARAM(hksel)), None);
    dark_theme_combo(ahk);
    restyle::dark_combo_subclass(ahk, ID_SHOT_ACTION_HK);
    // The Ctrl+S save folder: a read-only path display + the picker button. (The "Save to
    // a set folder" toggle lives up with the checkboxes.) Both grey out while that toggle is
    // off — see `update_save_dir_enabled`. The display seeds in load_values; the button
    // persists the pick immediately.
    lc.status(ID_SHOT_DIR);
    lc.button(t("btn_set_save_dir"), 150, ID_SHOT_SET_DIR);
    // Opens the user-editable upload-hosts config (the "Upload (copy link)" verb +
    // the capture overlay's Upload button POST through this chain of keyless hosts).
    lc.button(t("btn_edit_upload_hosts"), 184, ID_EDIT_UPLOAD_HOSTS);
    // Every link uploaded through that chain, with the time each has left.
    lc.button(t("btn_recent_uploads"), 184, ID_UPLOAD_HISTORY);
    // Live status of the background hotkey daemon + a Start/Restart button. The
    // hotkey does nothing unless this tray helper is running, so make it visible
    // and recoverable (seeded in load_values + refreshed on Restart).
    lc.status(ID_SHOT_STATUS);
    // Right-align the service status so it reads as a badge on the right; its word is
    // tinted green (running) / red (otherwise) in the WM_CTLCOLORSTATIC handler.
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_SHOT_STATUS) {
        const SS_RIGHT: u32 = 0x0002; // static right-align style (not surfaced by windows-rs here)
        let st = GetWindowLongW(h, GWL_STYLE) as u32 | SS_RIGHT;
        SetWindowLongW(h, GWL_STYLE, st as i32);
    }
    lc.button(t("btn_restart_hotkey"), 184, ID_SHOT_RESTART);
}

/// Diagnostics
pub(super) unsafe fn build_diagnostics(lc: &mut LeftCol, sty: &Styles) {
    // A user-sendable log of errors + crashes (a panic hook captures crashes before the
    // process aborts). "Verbose logging" flips the HKCU Debug DWORD so detailed traces
    // are written too; "Open diagnostics log" reveals the file for the user to send in.
    lc.header(t("grp_diagnostics"), sty.hdr, ID_LBL_DIAG, false);
    lc.checkbox(t("chk_verbose_log"), sty.cb, 300, ID_VERBOSE_LOG);
    lc.button(t("btn_open_log"), 184, ID_OPEN_LOG);
    lc.button(t("btn_rebuild_cache"), 184, ID_REBUILD_CACHE);
    lc.button(t("btn_repair_assoc"), 184, ID_REPAIR_ASSOC);
    // The self-check. Listed last in Diagnostics because it is the one you reach for FIRST
    // when something is wrong: it tells you which of the others (if any) is worth pressing.
    lc.button(t("btn_run_doctor"), 184, ID_RUN_DOCTOR);
    // Background update check (default ON; only acts while the resident hotkey helper
    // runs — no separate scheduled task). The manual button below works regardless.
    lc.checkbox(t("chk_update_auto"), sty.cb, 300, ID_UPDATE_AUTO);
    lc.button(t("btn_check_updates"), 184, ID_CHECK_UPDATES);
}

/// Settings sync (optional, opt-in)
pub(super) unsafe fn build_sync(lc: &mut LeftCol, sty: &Styles) {
    // Sign in with a Connections account to sync portable preferences across machines.
    // OFF by default — NO network happens unless the user clicks this. Only the
    // allowlisted prefs sync (never file paths, secrets, or per-machine state); see
    // `sync_client::ALLOW`.
    lc.header(t("sync_title"), sty.hdr, ID_LBL_SYNC, false);
    // A green "● Synced · up to date" badge (or a muted invite when signed out) sits on the
    // left of the row; the button ("Stop syncing" / "Sync settings…") is right-aligned. Both
    // are seeded in refresh_sync_ui — NO raw account id ever lands in the button label.
    lc.status(ID_SYNC_STATUS);
    lc.button(&sync_button_label(), 300, ID_SYNC_BTN);
}

/// Quick preview (QuickLook-style "press Space, see the file")
pub(super) unsafe fn build_quick_preview(lc: &mut LeftCol, sty: &Styles) {
    // The master toggle drives daemon residency (like the screenshot service — see
    // apply_settings, which persists it before the reconcile); the rest are viewer
    // behavior prefs. All are placed into the "Quick preview" nav category by cat_rows.
    lc.checkbox(t("chk_preview_enabled"), sty.cb, 312, ID_PREVIEW_ENABLED);
    lc.checkbox(
        t("chk_preview_hold_peek"),
        sty.cb,
        312,
        ID_PREVIEW_HOLD_PEEK,
    );
    lc.checkbox(
        t("chk_preview_close_focus"),
        sty.cb,
        312,
        ID_PREVIEW_CLOSE_FOCUS,
    );
    lc.checkbox(t("chk_preview_topmost"), sty.cb, 312, ID_PREVIEW_TOPMOST);
    // Per-extension blocklist: a free-text edit (NOT `edit_style` above — that forces
    // ES_NUMBER), same wide-single-line shape as the licence key / settings-search boxes.
    let blocked_exts_style = WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_TABSTOP;
    lc.edit(
        t("lbl_preview_blocked_exts"),
        ID_LBL_PREVIEW_BLOCKED_EXTS,
        blocked_exts_style,
        ID_PREVIEW_BLOCKED_EXTS,
    );
    lc.checkbox(t("chk_preview_text"), sty.cb, 312, ID_PREVIEW_TEXT);
    lc.checkbox(t("chk_preview_markdown"), sty.cb, 312, ID_PREVIEW_MARKDOWN);
    #[cfg(feature = "html-preview")]
    lc.checkbox(t("chk_preview_html"), sty.cb, 312, ID_PREVIEW_HTML);
    #[cfg(feature = "html-preview")]
    lc.checkbox(t("chk_preview_url_live"), sty.cb, 312, ID_PREVIEW_URL_LIVE);

    // Reset / Import / Export share one row. Reset sets every control to factory
    // defaults (the user clicks Save to persist, like any other change — the top-right
    // "Defaults" only resets the file-type list). Import/Export round-trip the whole
    // settings tree to a human-readable JSON file.
    lc.button_row(&[
        (t("btn_reset_all"), ID_RESET_ALL),
        (t("btn_import"), ID_IMPORT),
        (t("btn_export"), ID_EXPORT),
    ]);
}
