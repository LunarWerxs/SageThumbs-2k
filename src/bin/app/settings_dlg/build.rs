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
    lc.checkbox(t("chk_enable_thumbs"), cb, 300, ID_ENABLE_THUMBS);
    lc.checkbox(t("chk_prefer_embedded"), cb, 300, ID_USE_EMBEDDED);

    // Limits & quality — numeric label+edit rows. Single-line edits top-align +
    // ignore EM_SETRECT, so they're kept snug; the rounded field panel behind them
    // (biased up) supplies the box height and centers the digits.
    lc.header(t("grp_limits"), hdr, ID_LBL_LIMITS, false);
    lc.edit(t("lbl_max_file"), ID_LBL_MAXFILE, edit_style, ID_MAXSIZE);
    lc.edit(t("lbl_max_thumb"), ID_LBL_MAXTHUMB, edit_style, ID_SIZE);
    lc.edit(t("lbl_jpeg"), ID_LBL_JPEG, edit_style, ID_JPEG);
    lc.edit(t("lbl_png"), ID_LBL_PNG, edit_style, ID_PNG);

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
    lc.checkbox(t("chk_preserve_date"), cb, 312, ID_PRESERVE_DATE);
    let prev = lc.combo(t("lbl_menu_preview"), ID_LBL_PREVIEW, 160, ID_MENU_PREVIEW);
    for key in ["prev_off", "prev_submenu", "prev_main"] {
        let w = wide(t(key));
        SendMessageW(prev, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
    }
    SendMessageW(prev, CB_SETCURSEL, Some(WPARAM(settings::menu_preview() as usize)), None);
    // Widen the dropdown beyond the closed box so longer option labels (and longer
    // translations) aren't clipped.
    SendMessageW(prev, CB_SETDROPPEDWIDTH, Some(WPARAM(230)), None);
    dark_theme_combo(prev);
    restyle::dark_combo_subclass(prev, ID_MENU_PREVIEW);

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
        let _ = SetWindowPos(mlist, None, 0, 0, dpi_scale(hwnd, 322), needed_dev, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
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
    for &(label, _) in SHOT_PRESETS {
        let w = wide(label);
        SendMessageW(shot, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
    }
    // Select the preset matching the stored hotkey (default = first = Ctrl+PrtScn).
    let (m, v) = settings::screenshot_hotkey();
    let packed = (m << 8) | v;
    let sel = SHOT_PRESETS.iter().position(|&(_, p)| p == packed).unwrap_or(0);
    SendMessageW(shot, CB_SETCURSEL, Some(WPARAM(sel)), None);
    dark_theme_combo(shot);
    restyle::dark_combo_subclass(shot, ID_SHOT_HOTKEY);
    // Quick-save hotkey picker — grouped directly under the capture-hotkey combo.
    // Gated by the "instant screenshot" checkbox above (see `update_quick_enabled`);
    // greyed out while that box is unchecked.
    let quick = lc.combo(t("lbl_shot_quick_hotkey"), ID_LBL_SHOT_QUICK_HK, 200, ID_SHOT_QUICK_HOTKEY);
    for &(label, _) in SHOT_PRESETS {
        let w = wide(label);
        SendMessageW(quick, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
    }
    // Select the saved chord, or default to one that won't collide with the main
    // Ctrl+PrtScn, so flipping the checkbox on just works.
    let (qm, qv) = settings::screenshot_quick_hotkey();
    let qpacked = (qm << 8) | qv;
    let qsel = if qpacked == 0 {
        SHOT_PRESETS.iter().position(|&(l, _)| l == QUICK_DEFAULT_LABEL).unwrap_or(0)
    } else {
        SHOT_PRESETS.iter().position(|&(_, p)| p == qpacked).unwrap_or(0)
    };
    SendMessageW(quick, CB_SETCURSEL, Some(WPARAM(qsel)), None);
    dark_theme_combo(quick);
    restyle::dark_combo_subclass(quick, ID_SHOT_QUICK_HOTKEY);
    // Custom action hotkey: ONE user-assignable [action] + [hotkey] binding (the owner's
    // "two dropdowns" request). The chosen action fires from a global hotkey owned by this
    // same daemon. The action combo lists the curated `hotkey::ACTIONS`; the hotkey combo is
    // a "(none)" entry + the SHOT_PRESETS chords, where "(none)" = unbound. Seeded inline
    // from settings; persisted in apply_settings; reset in load_defaults.
    let act = lc.combo(t("lbl_custom_action"), ID_LBL_SHOT_ACTION, 200, ID_SHOT_ACTION);
    for &(_, label) in crate::hotkey::ACTIONS {
        let w = wide(label);
        SendMessageW(act, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
    }
    let cur_action = settings::custom_action();
    let asel = crate::hotkey::ACTIONS.iter().position(|&(id, _)| id == cur_action).unwrap_or(0);
    SendMessageW(act, CB_SETCURSEL, Some(WPARAM(asel)), None);
    dark_theme_combo(act);
    restyle::dark_combo_subclass(act, ID_SHOT_ACTION);
    // Its hotkey: item 0 is "(none)" (unbound); items 1.. mirror SHOT_PRESETS.
    let ahk = lc.combo(t("lbl_custom_action_hk"), ID_LBL_SHOT_ACTION_HK, 220, ID_SHOT_ACTION_HK);
    let none_w = wide(t("opt_none_unassigned"));
    SendMessageW(ahk, CB_ADDSTRING, None, Some(LPARAM(none_w.as_ptr() as isize)));
    for &(label, _) in SHOT_PRESETS {
        let w = wide(label);
        SendMessageW(ahk, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
    }
    let (cam, cav) = settings::custom_action_hotkey();
    let cpacked = (cam << 8) | cav;
    let hksel = if cav == 0 {
        0
    } else {
        SHOT_PRESETS.iter().position(|&(_, p)| p == cpacked).map_or(0, |i| i + 1)
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
    // Background update check (default ON; only acts while the resident hotkey helper
    // runs — no separate scheduled task). The manual button below works regardless.
    lc.checkbox(t("chk_update_auto"), cb, 300, ID_UPDATE_AUTO);
    lc.button(t("btn_check_updates"), 184, ID_CHECK_UPDATES);

    // ===== Settings sync (optional, opt-in) =====
    // Sign in with a Connections account to sync portable preferences across machines.
    // OFF by default — NO network happens unless the user clicks this. Only the
    // allowlisted prefs sync (never file paths, secrets, or per-machine state); see
    // `sync_client::ALLOW`. English-only for now (locale keys are a v1.1 follow-up).
    lc.header("Settings sync", hdr, ID_LBL_SYNC, false);
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
    lc.checkbox(t("chk_preview_close_focus"), cb, 312, ID_PREVIEW_CLOSE_FOCUS);
    lc.checkbox(t("chk_preview_topmost"), cb, 312, ID_PREVIEW_TOPMOST);
    lc.checkbox(t("chk_preview_text"), cb, 312, ID_PREVIEW_TEXT);
    lc.checkbox(t("chk_preview_markdown"), cb, 312, ID_PREVIEW_MARKDOWN);
    lc.checkbox(t("chk_preview_md_remote"), cb, 312, ID_PREVIEW_MD_REMOTE);
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
    ctl(hwnd, STATIC, t("lbl_formats"), hdr, rx, 12, 356, 18, ID_LBL_FORMATS, hinst);
    ctl(hwnd, BUTTON, t("btn_select_all"), WS_TABSTOP, rx, 34, 84, 26, ID_SELECT_ALL, hinst);
    ctl(hwnd, BUTTON, t("btn_clear_all"), WS_TABSTOP, rx + 90, 34, 84, 26, ID_CLEAR_ALL, hinst);
    ctl(hwnd, BUTTON, t("btn_defaults"), WS_TABSTOP, rx + 180, 34, 84, 26, ID_DEFAULTS, hinst);

    // Live search box (filters the list as you type). Borderless + rounded panel in
    // dark mode (like the other inputs); native bordered edit in light mode.
    let search_style = WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_TABSTOP;
    let search = ctl(hwnd, EDIT, "", search_style, rx, 70, 356, 18, ID_SEARCH, hinst);
    let cue = wide(t("search_formats"));
    SendMessageW(search, EM_SETCUEBANNER, Some(WPARAM(1)), Some(LPARAM(cue.as_ptr() as isize)));

    // Dark mode drops the square WS_BORDER — a rounded card frame is drawn behind
    // the list in WM_PAINT. Light mode keeps the native border.
    let list_style = WINDOW_STYLE(LVS_REPORT | LVS_NOSORTHEADER) | WS_TABSTOP;
    // Shorter list in dark mode (scrollable left column lets the window be shorter);
    // y=98 leaves room (with padding) for the search box above. Dark bottom = 442.
    let list_h = 344;
    let list = ctl(hwnd, WC_LISTVIEWW, "", list_style, rx, 98, 356, list_h, ID_LIST, hinst);
    SendMessageW(
        list,
        LVM_SETEXTENDEDLISTVIEWSTYLE,
        Some(WPARAM(0)),
        Some(LPARAM((LVS_EX_CHECKBOXES | LVS_EX_FULLROWSELECT) as isize)),
    );
    // Lift the list onto SURFACE() (a card) so the zebra alternates against it —
    // theme-aware: a white card in light, a near-black one in dark.
    SendMessageW(list, LVM_SETBKCOLOR, None, Some(LPARAM(SURFACE().0 as isize)));
    SendMessageW(list, LVM_SETTEXTBKCOLOR, None, Some(LPARAM(SURFACE().0 as isize)));
    SendMessageW(list, LVM_SETTEXTCOLOR, None, Some(LPARAM(DARK_TEXT().0 as isize)));
    if is_dark() {
        // Native dark item-view theme is dark-only; light keeps the native light header.
        let header = HWND(SendMessageW(list, LVM_GETHEADER, None, None).0 as *mut c_void);
        dark_control(header, w!("DarkMode_ItemsView"));
    }
    // Subclass for dark header text + SPACE/right-click bulk checkbox toggle.
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
        *s.borrow_mut() = formats::FORMATS.iter().map(|&(ext, _)| settings::format_enabled(ext)).collect();
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
        let _ = SetWindowSubclass(scroll, Some(restyle::scrollbar_subclass), ID_SCROLLBAR as usize, 0);
        // Full-width, owner-drawn (opaque) mask below the viewport — hides scrolled
        // controls + their field panels, and draws the divider above the banner.
        ctl(hwnd, STATIC, "", WINDOW_STYLE(SS_OWNERDRAW), 0, LEFT_VIEW_BOTTOM, 730, 70, ID_LEFT_MASK, hinst);
    }

    // ===== Sponsor promotion =====
    // Centered clickable banner (the product push), loaded from a remote URL at
    // runtime so it can change without a rebuild. SS_NOTIFY -> STN_CLICKED.
    // SS_REALSIZECONTROL pins the banner at 440×56 and fits the image to it — so an
    // oversized remote sponsor image can't grow the static over the footer buttons.
    //
    // The banner is gated on the REMOTE feed: `sponsors_enabled()` does a bounded,
    // cached fetch of the manifest and is true only if the feed is reachable, not
    // kill-switched (`"enabled": false`), and lists ≥1 sponsor. When off we never
    // create the banner control (every banner message handler already no-ops when
    // `GetDlgItem(ID_BANNER)` finds nothing) AND the footer rises into the banner's
    // slot so no empty gap is left; the outer window height (main.rs) is derived from
    // the same layout helper, so the window opens at the right size with no reflow.
    // The no-sponsor footer y == the banner's y by design (footer takes its slot).
    let sponsors_on = sponsors_enabled();
    let layout = sponsor_layout(is_dark(), sponsors_on);
    if sponsors_on {
        let banner = ctl(
            hwnd,
            STATIC,
            "",
            WINDOW_STYLE(SS_BITMAP | SS_NOTIFY | SS_REALSIZECONTROL),
            138,
            layout.banner_y,
            440,
            56,
            ID_BANNER,
            hinst,
        );
        // Placeholder fills the reserved space while the real sponsor art downloads
        // (the gate already confirmed sponsors exist), then gets swapped for it.
        if let Some(hbmp) = load_art(BANNER_PNG, "banner.png", 440, 56) {
            set_static_bitmap(banner, hbmp);
        }
        spawn_remote_sponsors(banner, 440, 56);
    }

    // ===== Bottom row: About + credit (left), inline with Save / Cancel (right) =====
    ctl(hwnd, BUTTON, t("btn_about"), WS_TABSTOP, MARGIN, layout.foot_y, 96, BTN_H, ID_ABOUT, hinst);
    let credit = format!("{} <a href=\"{URL_PARENT}\">Lunarwerx</a>", t("promo_made_by"));
    ctl(hwnd, SYSLINK, &credit, WS_TABSTOP, 122, layout.credit_y, 240, 20, ID_PROMO_LINK, hinst);
    // Cancel (secondary) on the left, Save (primary, wider + accent) rightmost —
    // a clear prominence/size difference, matching the mockup.
    ctl(hwnd, BUTTON, t("btn_cancel"), WS_TABSTOP, 508, layout.foot_y, 92, BTN_H, IDCANCEL, hinst);
    ctl(hwnd, BUTTON, t("btn_ok"), WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP, 608, layout.foot_y, 104, BTN_H, IDOK, hinst);

    // v3 reorg extras (repositioned by apply_v3_layout): the custom-action enable
    // toggle + the new group sub-headers for the merged General / regrouped Advanced.
    ctl(hwnd, BUTTON, t("chk_custom_action"), cb, 0, 0, 300, 20, ID_CUSTOM_ACTION_ENABLE, hinst);
    ctl(hwnd, STATIC, t("grp_updates"), hdr, 0, 0, 322, 18, ID_LBL_UPDATES, hinst);
    ctl(hwnd, STATIC, t("grp_backup"), hdr, 0, 0, 322, 18, ID_LBL_BACKUP, hinst);
    ctl(hwnd, STATIC, t("grp_hotkey_svc"), hdr, 0, 0, 322, 18, ID_LBL_HOTKEY_SVC, hinst);

    set_window_title(hwnd);
    load_values(hwnd);
    // The custom-action toggle reflects whether a hotkey is bound; it gates the two combos.
    check(hwnd, ID_CUSTOM_ACTION_ENABLE, settings::custom_action_hotkey().1 != 0);
    update_custom_action_enabled(hwnd);
    add_tooltips(hwnd, hinst);
    // v3 layout: relocate the controls created above into a category nav-rail +
    // content-pane shell (replacing the single scrolling column). Done as a
    // post-creation reposition so all the seeding/combo/list logic stays intact.
    apply_v3_layout(hwnd, hinst);
}

