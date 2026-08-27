//! The big `build_controls` — creates every dialog control (extracted from settings_dlg).

use super::*;

pub(super) unsafe fn build_controls(hwnd: HWND, hinst: HINSTANCE) {
    let cb = WINDOW_STYLE(BS_AUTOCHECKBOX as u32);
    // Dark mode: borderless, right-aligned number fields (a rounded field frame is
    // drawn behind them in WM_PAINT). Light mode: the original bordered,
    // left-aligned native edits.
    let edit_style = WINDOW_STYLE((ES_NUMBER | ES_AUTOHSCROLL | ES_RIGHT) as u32) | WS_TABSTOP;
    // Section headers owner-draw (uppercase label + hairline divider) in dark
    // mode; light mode keeps the plain native label but with SS_NOPREFIX so a
    // localized '&' (e.g. "Limits & quality") isn't eaten as a mnemonic. The
    // width is widened so the dark-mode divider runs to the column edge.
    let hdr = WINDOW_STYLE(SS_OWNERDRAW);

    // ===== Left column: options — one vertical rhythm via the LeftCol cursor =====
    let mut lc = LeftCol::new(hwnd, hinst);

    lc.header(t("grp_thumbnails"), hdr, ID_LBL_THUMBS, true);
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
    lc.checkbox(t("chk_enable_thumbs"), cb, 300, ID_ENABLE_THUMBS);
    lc.checkbox(t("chk_prefer_embedded"), cb, 300, ID_USE_EMBEDDED);
    lc.checkbox(t("chk_badge_icon"), cb, 300, ID_BADGE_ICON);
    lc.checkbox(t("chk_thumb_checker"), cb, 300, ID_THUMB_CHECKER);
    lc.checkbox(t("chk_video_cover_art"), cb, 300, ID_VIDEO_COVER_ART);
    // Headers the v3 layout places, not this legacy column: File types' two sections,
    // the menu page's verb-behavior split, and Quick preview's content-type split.
    lc.header(t("grp_tile_look"), hdr, ID_LBL_TILE_LOOK, false);
    lc.header(t("grp_formats_pick"), hdr, ID_LBL_FORMATS_PICK, false);
    lc.header(t("grp_convert_verbs"), hdr, ID_LBL_CONVERT_VERBS, false);
    lc.header(t("grp_preview_kinds"), hdr, ID_LBL_PREVIEW_KINDS, false);
    lc.header(t("grp_menu_look"), hdr, ID_LBL_MENU_LOOK, false);
    lc.header(t("grp_quickaction"), hdr, ID_LBL_QUICKACTION, false);
    lc.header(
        t("grp_preview_behavior"),
        hdr,
        ID_LBL_PREVIEW_BEHAVIOR,
        false,
    );
    lc.button(t("btn_menu_items_edit"), 200, ID_MENU_ITEMS_EDIT);

    // Limits & quality — numeric label+edit rows. Single-line edits top-align +
    // ignore EM_SETRECT, so they're kept snug; the rounded field panel behind them
    // (biased up) supplies the box height and centers the digits.
    lc.header(t("grp_limits"), hdr, ID_LBL_LIMITS, false);
    lc.edit(t("lbl_max_file"), ID_LBL_MAXFILE, edit_style, ID_MAXSIZE);
    lc.edit(t("lbl_max_thumb"), ID_LBL_MAXTHUMB, edit_style, ID_SIZE);
    lc.edit(t("lbl_jpeg"), ID_LBL_JPEG, edit_style, ID_JPEG);
    lc.edit(t("lbl_png"), ID_LBL_PNG, edit_style, ID_PNG);
    // Created with the other numeric rows; `navrail::cat_rows` puts it on Appearance, next to
    // the video cover-art switch it belongs with (creation order carries no meaning here).
    lc.edit(
        t("lbl_video_offset"),
        ID_LBL_VIDEO_OFFSET,
        edit_style,
        ID_VIDEO_OFFSET,
    );

    // Ebook & comic archive cover options (the DarkThumbs toggles).
    lc.header(t("grp_ebook"), hdr, ID_LBL_EBOOK, false);
    lc.checkbox(t("chk_sort"), cb, 312, ID_C_SORT);
    lc.checkbox(t("chk_prefer_cover"), cb, 312, ID_C_PREFER_COVER);
    lc.checkbox(t("chk_skip_scanlation"), cb, 312, ID_C_SKIP_SCAN);
    lc.checkbox(t("chk_archive_sheet"), cb, 312, ID_C_ARCHIVE_SHEET);

    // ===== General: right-click menu integration + UI language =====
    // Menu toggles grouped as checkboxes, then the two dropdowns below them.
    lc.header(t("grp_lang_files"), hdr, ID_LBL_GENERAL, false);
    lc.checkbox(t("chk_enable_menu"), cb, 300, ID_ENABLE_MENU);
    lc.checkbox(t("chk_menu_all_types"), cb, 300, ID_MENU_ALL_TYPES);
    lc.checkbox(t("chk_menu_quick"), cb, 312, ID_MENU_QUICK);
    lc.checkbox(t("chk_menu_checker"), cb, 300, ID_MENU_CHECKER);
    lc.checkbox(t("chk_folder_prebuild"), cb, 312, ID_FOLDER_PREBUILD);
    lc.checkbox(t("chk_preserve_date"), cb, 312, ID_PRESERVE_DATE);
    lc.checkbox(t("chk_keep_metadata"), cb, 312, ID_KEEP_METADATA);
    lc.checkbox(t("chk_pdf_margin"), cb, 312, ID_PDF_MARGIN);
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

    // ===== Menu items: show/hide each SageThumbs 2K context-menu entry =====
    // XnShell-style "Displayed menu items" checklist; each label reuses the menu
    // item's own translated name. (Settings is always shown, so it isn't listed.)
    lc.header(t("grp_menu_items"), hdr, ID_LBL_MENU_ITEMS, false);
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

    // ===== Screenshots: capture service + hotkey =====
    // The opt-in screen-capture controls (enable toggle + hotkey preset). The enable
    // checkbox seeds in load_values; the picker seeds inline from the stored hotkey.
    lc.header(t("grp_screenshots"), hdr, ID_LBL_SHOT, false);
    lc.checkbox(t("chk_screenshot"), cb, 300, ID_SHOT_ENABLE);
    // Owner layout pref: group the screenshot CHECKBOXES together, then the hotkey
    // DROPDOWNS together below. The instant-screenshot checkbox gates the Quick-save
    // combo further down (that combo greys out while this is unchecked).
    lc.checkbox(t("chk_hide_tray"), cb, 300, ID_SHOT_HIDE_TRAY);
    lc.checkbox(t("chk_instant_screenshot"), cb, 300, ID_SHOT_QUICK_ENABLE);
    // Ctrl+S destination toggle — kept WITH the other screenshot checkboxes (owner pref:
    // checkboxes grouped, then dropdowns). On → auto-save to the fixed folder below
    // (Desktop by default); off → Ctrl+S prompts each time. (Ctrl+C always copies.)
    lc.checkbox(t("chk_shot_use_dir"), cb, 300, ID_SHOT_USE_DIR);
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
    for &(label, packed) in SHOT_PRESETS {
        let w = wide(label);
        let idx = SendMessageW(ahk, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize))).0;
        SendMessageW(
            ahk,
            CB_SETITEMDATA,
            Some(WPARAM(idx as usize)),
            Some(LPARAM(packed as isize)),
        );
    }
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

    // ===== Diagnostics =====
    // A user-sendable log of errors + crashes (a panic hook captures crashes before the
    // process aborts). "Verbose logging" flips the HKCU Debug DWORD so detailed traces
    // are written too; "Open diagnostics log" reveals the file for the user to send in.
    lc.header(t("grp_diagnostics"), hdr, ID_LBL_DIAG, false);
    lc.checkbox(t("chk_verbose_log"), cb, 300, ID_VERBOSE_LOG);
    lc.button(t("btn_open_log"), 184, ID_OPEN_LOG);
    lc.button(t("btn_rebuild_cache"), 184, ID_REBUILD_CACHE);
    lc.button(t("btn_repair_assoc"), 184, ID_REPAIR_ASSOC);
    // The self-check. Listed last in Diagnostics because it is the one you reach for FIRST
    // when something is wrong: it tells you which of the others (if any) is worth pressing.
    lc.button(t("btn_run_doctor"), 184, ID_RUN_DOCTOR);
    // Background update check (default ON; only acts while the resident hotkey helper
    // runs — no separate scheduled task). The manual button below works regardless.
    lc.checkbox(t("chk_update_auto"), cb, 300, ID_UPDATE_AUTO);
    lc.button(t("btn_check_updates"), 184, ID_CHECK_UPDATES);

    // ===== Settings sync (optional, opt-in) =====
    // Sign in with a Connections account to sync portable preferences across machines.
    // OFF by default — NO network happens unless the user clicks this. Only the
    // allowlisted prefs sync (never file paths, secrets, or per-machine state); see
    // `sync_client::ALLOW`.
    lc.header(t("sync_title"), hdr, ID_LBL_SYNC, false);
    // A green "● Synced · up to date" badge (or a muted invite when signed out) sits on the
    // left of the row; the button ("Stop syncing" / "Sync settings…") is right-aligned. Both
    // are seeded in refresh_sync_ui — NO raw account id ever lands in the button label.
    lc.status(ID_SYNC_STATUS);
    lc.button(&sync_button_label(), 300, ID_SYNC_BTN);

    // ===== Quick preview (QuickLook-style "press Space, see the file") =====
    // The master toggle drives daemon residency (like the screenshot service — see
    // apply_settings, which persists it before the reconcile); the rest are viewer
    // behavior prefs. All are placed into the "Quick preview" nav category by cat_rows.
    lc.checkbox(t("chk_preview_enabled"), cb, 312, ID_PREVIEW_ENABLED);
    lc.checkbox(t("chk_preview_hold_peek"), cb, 312, ID_PREVIEW_HOLD_PEEK);
    lc.checkbox(
        t("chk_preview_close_focus"),
        cb,
        312,
        ID_PREVIEW_CLOSE_FOCUS,
    );
    lc.checkbox(t("chk_preview_topmost"), cb, 312, ID_PREVIEW_TOPMOST);
    lc.checkbox(t("chk_preview_text"), cb, 312, ID_PREVIEW_TEXT);
    lc.checkbox(t("chk_preview_markdown"), cb, 312, ID_PREVIEW_MARKDOWN);
    #[cfg(feature = "html-preview")]
    lc.checkbox(t("chk_preview_html"), cb, 312, ID_PREVIEW_HTML);
    #[cfg(feature = "html-preview")]
    lc.checkbox(t("chk_preview_url_live"), cb, 312, ID_PREVIEW_URL_LIVE);

    // Reset / Import / Export share one row. Reset sets every control to factory
    // defaults (the user clicks Save to persist, like any other change — the top-right
    // "Defaults" only resets the file-type list). Import/Export round-trip the whole
    // settings tree to a human-readable JSON file.
    lc.button_row(&[
        (t("btn_reset_all"), ID_RESET_ALL),
        (t("btn_import"), ID_IMPORT),
        (t("btn_export"), ID_EXPORT),
    ]);

    // ===== Right column: supported file types =====
    let rx = 348;
    ctl(
        hwnd,
        STATIC,
        t("lbl_formats"),
        hdr,
        rx,
        12,
        356,
        18,
        ID_LBL_FORMATS,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_select_all"),
        WS_TABSTOP,
        rx,
        34,
        84,
        26,
        ID_SELECT_ALL,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_clear_all"),
        WS_TABSTOP,
        rx + 90,
        34,
        84,
        26,
        ID_CLEAR_ALL,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_defaults"),
        WS_TABSTOP,
        rx + 180,
        34,
        84,
        26,
        ID_DEFAULTS,
        hinst,
    );

    // Live search box (filters the list as you type). Borderless + rounded panel in
    // dark mode (like the other inputs); native bordered edit in light mode.
    let search_style = WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_TABSTOP;
    let search = ctl(
        hwnd,
        EDIT,
        "",
        search_style,
        rx,
        70,
        356,
        18,
        ID_SEARCH,
        hinst,
    );
    let cue = wide(t("search_formats"));
    SendMessageW(
        search,
        EM_SETCUEBANNER,
        Some(WPARAM(1)),
        Some(LPARAM(cue.as_ptr() as isize)),
    );

    // Dark mode drops the square WS_BORDER — a rounded card frame is drawn behind
    // the list in WM_PAINT. Light mode keeps the native border.
    let list_style = WINDOW_STYLE(LVS_REPORT | LVS_NOSORTHEADER) | WS_TABSTOP;
    // Shorter list in dark mode (scrollable left column lets the window be shorter);
    // y=98 leaves room (with padding) for the search box above. Dark bottom = 442.
    let list_h = 344;
    let list = ctl(
        hwnd,
        WC_LISTVIEWW,
        "",
        list_style,
        rx,
        98,
        356,
        list_h,
        ID_LIST,
        hinst,
    );
    SendMessageW(
        list,
        LVM_SETEXTENDEDLISTVIEWSTYLE,
        Some(WPARAM(0)),
        Some(LPARAM((LVS_EX_CHECKBOXES | LVS_EX_FULLROWSELECT) as isize)),
    );
    // Lift the list onto SURFACE() (a card) so the zebra alternates against it —
    // theme-aware: a white card in light, a near-black one in dark.
    SendMessageW(
        list,
        LVM_SETBKCOLOR,
        None,
        Some(LPARAM(SURFACE().0 as isize)),
    );
    SendMessageW(
        list,
        LVM_SETTEXTBKCOLOR,
        None,
        Some(LPARAM(SURFACE().0 as isize)),
    );
    SendMessageW(
        list,
        LVM_SETTEXTCOLOR,
        None,
        Some(LPARAM(DARK_TEXT().0 as isize)),
    );
    let header = HWND(SendMessageW(list, LVM_GETHEADER, None, None).0 as *mut c_void);
    // COLUMNS ARE DRAG-RESIZABLE. This used to OR in `HDS_NOSIZING`, on the reasoning that
    // `fit_columns` already sized Description to exactly fill the list so a drag could only
    // truncate it or open a dead gap. That reasoning covered the LAST column and quietly took
    // the other two with it: Extension and Category are fixed at 64 and 92 px, which is not
    // enough for their own labels in a long language, and no amount of window resizing helps
    // because only Description grows. The reporter of issue #26.3 could read neither.
    //
    // Sizing is back on for THE FIRST TWO, and `fit_columns` now measures them instead of
    // assuming their widths, so widening Extension reflows Description and the total still
    // exactly fills the list.
    //
    // DESCRIPTION ITSELF IS REFUSED, in `list::list_subclass` via HDN_BEGINTRACK. It is the last
    // column and it is auto-fitted to fill, so a drag can only shrink it and leave dead space
    // against the scrollbar — which looks like a rendering fault, not a layout the user chose.
    // An earlier attempt allowed the drag and turned the auto-fit off to stop it snapping back;
    // that traded a snap-back for a permanent gap, so the drag is simply not offered now.
    //
    // The last column's right-hand divider is also painted out in `list::list_subclass`: it sits
    // at the far edge of the list where there is nothing to drag INTO. The INNER dividers — the
    // ones that do something — are left alone.
    if is_dark() {
        // Native dark item-view theme is dark-only; light keeps the native light header.
        dark_control(header, w!("DarkMode_ItemsView"));
    }
    // Subclass for dark header text, the column-drag reflow, and the SPACE/right-click bulk
    // checkbox toggle.
    let _ = SetWindowSubclass(list, Some(list::list_subclass), 0, 0);
    // Extension | Category | Description. FORMATS is ordered by category, so the
    // list naturally clusters: Images, then Camera RAW, then Ebooks & comics —
    // and the Category column labels each (robust in dark mode, unlike native
    // ListView group headers, which the dark theme refuses to render).
    insert_column(list, 0, t("col_extension"), 64);
    insert_column(list, 1, t("col_category"), 92);
    insert_column(list, 2, t("col_description"), 196);

    // The per-format checked state lives in a model (FMT_STATE), not the list —
    // so the search can rebuild the list view without losing toggles. Seed it from
    // settings, then populate the (unfiltered) view.
    FMT_STATE.with(|s| {
        *s.borrow_mut() = formats::FORMATS
            .iter()
            .map(|&(ext, _)| settings::format_enabled(ext))
            .collect();
    });
    populate_list(list, "");

    // ===== Left-column scrollbar + clipping mask =====
    // The vertical scrollbar for the left options, plus an opaque mask just below
    // the viewport that hides any control scrolled out of view (so it can't bleed
    // over the banner / footer). Created after the left controls so they sit on
    // top of them, but before the banner/footer so those sit on top of the mask.
    // Both themes: light is a recolored clone of dark, so it scrolls too.
    {
        let scroll = ctl(
            hwnd,
            w!("SCROLLBAR"),
            "",
            WINDOW_STYLE(SBS_VERT as u32) | WS_TABSTOP,
            LEFT_RIGHT_EDGE - 14,
            LEFT_VIEW_TOP,
            14,
            LEFT_VIEW_BOTTOM - LEFT_VIEW_TOP,
            ID_SCROLLBAR,
            hinst,
        );
        let _ = SetWindowSubclass(
            scroll,
            Some(restyle::scrollbar_subclass),
            ID_SCROLLBAR as usize,
            0,
        );
        // Full-width, owner-drawn (opaque) mask below the viewport — hides scrolled
        // controls + their field panels, and draws the divider above the banner.
        ctl(
            hwnd,
            STATIC,
            "",
            WINDOW_STYLE(SS_OWNERDRAW),
            0,
            LEFT_VIEW_BOTTOM,
            730,
            70,
            ID_LEFT_MASK,
            hinst,
        );
    }

    // ===== Sponsor promotion =====
    // Centered clickable banner (the product push). SS_NOTIFY -> STN_CLICKED.
    // SS_REALSIZECONTROL pins the banner at 440×56 and fits an image to it.
    //
    // v3 nav-rail layout (`apply_v3_layout`, called at the end of this function)
    // unconditionally hides ID_BANNER on every page (`navrail::V3_ALWAYS_HIDDEN`)
    // with no page that ever un-hides it. Creating the (permanently invisible)
    // control itself is cheap and kept, so its message handlers keep a live
    // control to safely no-op against, same as every other permanently-hidden
    // v3 control — but the real cost, the remote art download/decode + rotator
    // timers, no longer runs at all: nothing loads a bitmap into it and
    // `spawn_remote_sponsors` (the download/decode pipeline) is never called
    // (A093/A264, 2026-08-15).
    //
    // `sponsors_enabled()` still runs: its manifest fetch is also where the
    // one-shot install/reinstall report gets sent (`manifest_bytes` in
    // `sponsors.rs`), and that side effect stays live regardless of whether the
    // banner shows. The boolean result itself is no longer needed for layout.
    let _ = sponsors_enabled();
    let layout = sponsor_layout(is_dark());
    ctl(
        hwnd,
        STATIC,
        "",
        WINDOW_STYLE(SS_BITMAP | SS_NOTIFY | SS_REALSIZECONTROL),
        138,
        460,
        440,
        56,
        ID_BANNER,
        hinst,
    );

    // ===== Bottom row: About + credit (left), inline with Save / Cancel (right) =====
    ctl(
        hwnd,
        BUTTON,
        t("btn_about"),
        WS_TABSTOP,
        MARGIN,
        layout.foot_y,
        96,
        BTN_H,
        ID_ABOUT,
        hinst,
    );
    let credit = format!(
        "{} <a href=\"{URL_PARENT}\">Lunarwerx</a>",
        t("promo_made_by")
    );
    ctl(
        hwnd,
        SYSLINK,
        &credit,
        WS_TABSTOP,
        122,
        layout.credit_y,
        240,
        20,
        ID_PROMO_LINK,
        hinst,
    );
    // Close (secondary) on the left, Save (primary, wider + accent) rightmost —
    // a clear prominence/size difference, matching the mockup.
    // "Close", not "Cancel": Save applies immediately and leaves the window open, so
    // this button only dismisses it. Labelling it Cancel implied it would revert.
    // (`btn_cancel` stays for Convert / Files-to-folder / Tags-to-folders, which do
    // genuinely cancel an operation.)
    ctl(
        hwnd,
        BUTTON,
        t("btn_close"),
        WS_TABSTOP,
        508,
        layout.foot_y,
        92,
        BTN_H,
        IDCANCEL,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_ok"),
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
        608,
        layout.foot_y,
        104,
        BTN_H,
        IDOK,
        hinst,
    );

    // v3 reorg extras (repositioned by apply_v3_layout): the custom-action enable
    // toggle + the new group sub-headers for the merged General / regrouped Advanced.
    ctl(
        hwnd,
        BUTTON,
        t("chk_custom_action"),
        cb,
        0,
        0,
        300,
        20,
        ID_CUSTOM_ACTION_ENABLE,
        hinst,
    );
    ctl(
        hwnd,
        STATIC,
        t("grp_updates"),
        hdr,
        0,
        0,
        322,
        18,
        ID_LBL_UPDATES,
        hinst,
    );
    ctl(
        hwnd,
        STATIC,
        t("grp_backup"),
        hdr,
        0,
        0,
        322,
        18,
        ID_LBL_BACKUP,
        hinst,
    );
    ctl(
        hwnd,
        STATIC,
        t("grp_hotkey_svc"),
        hdr,
        0,
        0,
        322,
        18,
        ID_LBL_HOTKEY_SVC,
        hinst,
    );

    set_window_title(hwnd);
    load_values(hwnd);
    // The custom-action toggle reflects whether a hotkey is bound; it gates the two combos.
    check(
        hwnd,
        ID_CUSTOM_ACTION_ENABLE,
        settings::custom_action_hotkey().1 != 0,
    );
    update_custom_action_enabled(hwnd);
    add_tooltips(hwnd, hinst);
    // v3 layout: relocate the controls created above into a category nav-rail +
    // content-pane shell (replacing the single scrolling column). Done as a
    // post-creation reposition so all the seeding/combo/list logic stays intact.
    apply_v3_layout(hwnd, hinst);
}

/// Format a hotkey chord that isn't one of the curated [`SHOT_PRESETS`] — e.g. a
/// value an older preset list offered and has since dropped, or one written by
/// hand into the registry — so Save can round-trip it instead of silently
/// replacing it with preset 0. Deliberately plain (modifier names + a raw VK
/// hex byte), not a friendly key name: this is a recovery display for values
/// outside the curated list, not worth a `GetKeyNameTextW` round trip for.
fn describe_unknown_chord(packed: u32) -> String {
    let hkf = (packed >> 8) & 0xFF;
    let vk = packed & 0xFF;
    let mut mods = Vec::new();
    if hkf & 0x02 != 0 {
        mods.push("Ctrl");
    }
    if hkf & 0x01 != 0 {
        mods.push("Shift");
    }
    if hkf & 0x04 != 0 {
        mods.push("Alt");
    }
    if mods.is_empty() {
        format!("Custom (VK 0x{vk:02X})")
    } else {
        format!("Custom ({} + VK 0x{vk:02X})", mods.join(" + "))
    }
}

/// Append one combo item for a chord outside the curated list, with its packed
/// value stashed as the item's data (read back at Save via `CB_GETITEMDATA`
/// instead of re-deriving it from position). Returns the new item's index.
unsafe fn append_unknown_chord_item(combo: HWND, packed: u32) -> usize {
    let label = wide(&describe_unknown_chord(packed));
    let idx = SendMessageW(
        combo,
        CB_ADDSTRING,
        None,
        Some(LPARAM(label.as_ptr() as isize)),
    )
    .0;
    SendMessageW(
        combo,
        CB_SETITEMDATA,
        Some(WPARAM(idx as usize)),
        Some(LPARAM(packed as isize)),
    );
    idx as usize
}

/// Decide which combo index a stored chord should select: a curated preset's
/// index, `default_when_unset` when nothing is genuinely saved yet (`current ==
/// 0`), or `None` when `current` is a real value that just isn't in the curated
/// list — the caller must then append a dedicated item for it rather than
/// falling back to a default. This is the exact decision the original bug got
/// wrong (`SHOT_PRESETS.position(...).unwrap_or(0)` treated "unknown" and
/// "unset" as the same thing, both collapsing to preset 0).
fn preset_index_for(current: u32, default_when_unset: usize) -> Option<usize> {
    if current == 0 {
        return Some(default_when_unset);
    }
    SHOT_PRESETS.iter().position(|&(_, p)| p == current)
}

/// Populate a hotkey combo with the curated [`SHOT_PRESETS`] (each item's data =
/// its packed chord), append a trailing item for `current` when it's a real
/// (non-zero) chord absent from that list, and return the index to select —
/// `default_when_unset` when `current` is 0 (a combo-specific "nothing saved
/// yet" default; see callers). Save-time code reads the selection back with
/// `CB_GETITEMDATA`, so the appended item round-trips exactly like a curated
/// one instead of collapsing to preset 0 on the next Save (the bug this fixes).
unsafe fn populate_hotkey_presets(combo: HWND, current: u32, default_when_unset: usize) -> usize {
    for &(label, packed) in SHOT_PRESETS {
        let w = wide(label);
        let idx = SendMessageW(combo, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize))).0;
        SendMessageW(
            combo,
            CB_SETITEMDATA,
            Some(WPARAM(idx as usize)),
            Some(LPARAM(packed as isize)),
        );
    }
    match preset_index_for(current, default_when_unset) {
        Some(idx) => idx,
        None => append_unknown_chord_item(combo, current),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stored chord that isn't in `SHOT_PRESETS` must NOT resolve to the same
    /// index as "nothing saved" — that collapse (both cases returning
    /// `unwrap_or(0)`) is exactly what made Save silently replace a legacy/
    /// foreign chord with preset 0.
    #[test]
    fn unknown_chord_does_not_collapse_to_the_unset_default() {
        // A value that was never one of the curated presets.
        let foreign = (0x07 << 8) | 0x99; // Ctrl+Shift+Alt + an odd VK
        assert!(SHOT_PRESETS.iter().all(|&(_, p)| p != foreign));
        assert_eq!(preset_index_for(foreign, 0), None);
    }

    /// A genuinely unset chord (0 — no hotkey saved yet, e.g. the quick-save
    /// combo before the user ever touches it) still gets the caller's default.
    #[test]
    fn unset_chord_uses_the_caller_default() {
        assert_eq!(preset_index_for(0, 4), Some(4));
    }

    /// A stored chord that IS one of the curated presets resolves to that
    /// preset's own index, never to `default_when_unset`.
    #[test]
    fn known_chord_resolves_to_its_own_preset_index() {
        for (i, &(_, packed)) in SHOT_PRESETS.iter().enumerate() {
            assert_eq!(preset_index_for(packed, 99), Some(i));
        }
    }

    #[test]
    fn describe_unknown_chord_names_every_modifier() {
        assert_eq!(describe_unknown_chord(0x41), "Custom (VK 0x41)");
        assert_eq!(
            describe_unknown_chord((0x02 << 8) | 0x41),
            "Custom (Ctrl + VK 0x41)"
        );
        assert_eq!(
            describe_unknown_chord((0x07 << 8) | 0x41),
            "Custom (Ctrl + Shift + Alt + VK 0x41)"
        );
    }
}
