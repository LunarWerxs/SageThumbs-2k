//! Settings into controls: the saved values, the factory defaults and the combo selections that mirror them.

use super::*;

pub(in super::super) unsafe fn load_values(hwnd: HWND) {
    check(hwnd, ID_ENABLE_THUMBS, settings::thumbnails_enabled());
    check(hwnd, ID_USE_EMBEDDED, settings::use_embedded());
    set_combo(
        hwnd,
        ID_CORNER_MARK,
        settings::corner_mark().as_dword() as usize,
    );
    set_combo(
        hwnd,
        ID_BADGE_SIZE,
        settings::badge_size().as_dword() as usize,
    );
    check(hwnd, ID_BADGE_ICON, settings::format_badge_icon());
    check(hwnd, ID_THUMB_CHECKER, settings::thumb_checker());
    check(hwnd, ID_VIDEO_COVER_ART, settings::prefer_cover_art());
    update_badge_style_enabled(hwnd);
    check(hwnd, ID_ENABLE_MENU, settings::menu_enabled());
    check(hwnd, ID_MENU_ALL_TYPES, settings::menu_all_file_types());
    let mb = (settings::max_file_size_bytes() / (1024 * 1024)).min(u32::MAX as u64) as u32;
    let _ = SetDlgItemInt(hwnd, ID_MAXSIZE, mb, false);
    let _ = SetDlgItemInt(hwnd, ID_SIZE, settings::max_thumb_size(), false);
    let _ = SetDlgItemInt(hwnd, ID_JPEG, settings::jpeg_quality() as u32, false);
    let _ = SetDlgItemInt(hwnd, ID_PNG, settings::png_level(), false);
    let _ = SetDlgItemInt(hwnd, ID_VIDEO_OFFSET, settings::video_offset_pct(), false);
    check(hwnd, ID_C_SORT, settings::container_sort());
    check(hwnd, ID_C_PREFER_COVER, settings::container_prefer_cover());
    check(hwnd, ID_C_SKIP_SCAN, settings::container_skip_scanlation());
    check(hwnd, ID_C_ARCHIVE_SHEET, settings::archive_collage());
    check(hwnd, ID_MENU_QUICK, settings::menu_quick_verbs());
    check(hwnd, ID_MENU_CHECKER, settings::preview_checker());
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_APP_THEME) {
        SendMessageW(
            c,
            CB_SETCURSEL,
            Some(WPARAM(settings::app_theme() as usize)),
            None,
        );
    }
    check(hwnd, ID_FOLDER_PREBUILD, settings::folder_prebuild_verb());
    check(hwnd, ID_PRESERVE_DATE, settings::preserve_file_date());
    check(hwnd, ID_KEEP_METADATA, settings::keep_metadata_on_convert());
    check(
        hwnd,
        ID_PDF_MARGIN,
        !matches!(settings::pdf_page(), sagethumbs2k_core::PdfPage::Tight),
    );
    if let Ok(mlist) = GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST) {
        // Rows may be in a custom drag-reorder, so seed each ROW from its own key
        // (via lParam), not by a fixed toggle index.
        let count = SendMessageW(mlist, LVM_GETITEMCOUNT, None, None).0 as i32;
        for row in 0..count {
            if let Some(ti) = menu_row_toggle(mlist, row) {
                set_check(
                    mlist,
                    row,
                    settings::menu_item_shown(MENU_ITEM_TOGGLES[ti].1),
                );
            }
        }
    }
    // The screenshot toggle reflects the live service state (an HKCU autostart
    // entry), not a SageThumbs2K DWORD — so it's read separately.
    check(hwnd, ID_SHOT_ENABLE, crate::screenshot::is_enabled());
    check(hwnd, ID_SHOT_HIDE_TRAY, settings::screenshot_hide_tray());
    check(hwnd, ID_SHOT_USE_DIR, settings::screenshot_use_save_dir());
    set_shot_dir_label(hwnd);
    update_save_dir_enabled(hwnd);
    check(hwnd, ID_VERBOSE_LOG, settings::verbose_logging());
    check(hwnd, ID_UPDATE_AUTO, settings::update_auto_check());
    // Quick preview toggles.
    check(hwnd, ID_PREVIEW_ENABLED, settings::preview_enabled());
    check(hwnd, ID_PREVIEW_HOLD_PEEK, settings::preview_hold_peek());
    check(
        hwnd,
        ID_PREVIEW_CLOSE_FOCUS,
        settings::preview_close_on_focus_loss(),
    );
    check(hwnd, ID_PREVIEW_TOPMOST, settings::preview_open_front());
    // Round-trips exactly what the user typed (unparsed) — see `preview_blocked_exts_raw`'s
    // doc for why parsing happens only on read, never here.
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_PREVIEW_BLOCKED_EXTS) {
        let w = wide(&settings::preview_blocked_exts_raw());
        let _ = SetWindowTextW(c, PCWSTR(w.as_ptr()));
    }
    check(hwnd, ID_PREVIEW_TEXT, settings::preview_text());
    check(hwnd, ID_PREVIEW_MARKDOWN, settings::preview_markdown());
    #[cfg(feature = "html-preview")]
    check(hwnd, ID_PREVIEW_HTML, settings::preview_html());
    #[cfg(feature = "html-preview")]
    check(hwnd, ID_PREVIEW_URL_LIVE, settings::preview_url_live());
    // Instant screenshot is on iff a quick-save hotkey is stored (vk != 0); grey the
    // picker to match.
    check(
        hwnd,
        ID_SHOT_QUICK_ENABLE,
        settings::screenshot_quick_hotkey().1 != 0,
    );
    update_quick_enabled(hwnd);
    refresh_shot_status(hwnd);
    // Portable copies only lay this row out, but seeding it unconditionally is harmless and
    // keeps the "what does load_values touch" list free of a special case.
    set_portable_reg_state(hwnd);
    // Seed the Settings-sync row (button label + green "● Synced" badge) from the signed-in
    // state; the background pull (spawn_sync_pull) later refreshes it via WM_APP_SYNC.
    refresh_sync_ui(hwnd);
    // Seed the Licence page's two status lines + the idle redeem-result line; Redeem/Check
    // now later refresh them via WM_APP_LICENCE.
    licence_ui::seed_licence_ui(hwnd);
    // LAST, after every parent checkbox has its real value: grey (or un-grey) the dependent
    // rows. The earlier `update_badge_style_enabled` call runs before the menu/preview
    // masters are loaded and would leave their children greyed under an enabled parent.
    sync_dependent_switches(hwnd);
}

/// Reset every control to the factory defaults (does not write yet).
pub(in super::super) unsafe fn load_defaults(hwnd: HWND) {
    check(hwnd, ID_ENABLE_THUMBS, true);
    check(hwnd, ID_USE_EMBEDDED, true); // ON by default — see settings::use_embedded
                                        // Leave the corner to Windows: our mark alters the picture the user asked to see, and
                                        // hiding Explorer's writes into other programs' ProgID keys. Neither without being asked.
    set_combo(
        hwnd,
        ID_CORNER_MARK,
        settings::CornerMark::default().as_dword() as usize,
    );
    // The size the badge has always been drawn at: a bigger mark covers more of the picture,
    // so the steps above it are asked for, never defaulted into.
    set_combo(
        hwnd,
        ID_BADGE_SIZE,
        settings::BadgeSize::default().as_dword() as usize,
    );
    check(hwnd, ID_BADGE_ICON, true); // ...but when it IS on, colour beats three letters
    check(hwnd, ID_THUMB_CHECKER, false); // real alpha is the better default
    check(hwnd, ID_VIDEO_COVER_ART, false); // see settings::prefer_cover_art's documented default
    update_badge_style_enabled(hwnd);
    check(hwnd, ID_ENABLE_MENU, true);
    check(hwnd, ID_MENU_ALL_TYPES, false);
    let _ = SetDlgItemInt(hwnd, ID_MAXSIZE, settings::DEFAULT_MAX_FILE_MB, false);
    let _ = SetDlgItemInt(hwnd, ID_SIZE, settings::DEFAULT_THUMB_SIZE, false);
    let _ = SetDlgItemInt(hwnd, ID_JPEG, settings::DEFAULT_JPEG, false);
    let _ = SetDlgItemInt(hwnd, ID_PNG, settings::DEFAULT_PNG, false);
    let _ = SetDlgItemInt(
        hwnd,
        ID_VIDEO_OFFSET,
        settings::DEFAULT_VIDEO_OFFSET_PCT,
        false,
    );
    check(hwnd, ID_C_SORT, true);
    check(hwnd, ID_C_PREFER_COVER, true);
    // ON, like `settings::container_skip_scanlation`'s fallback and a clean install (Michael,
    // 2026-09-15); a reset used to switch it off and so differ from a fresh install (F08).
    check(hwnd, ID_C_SKIP_SCAN, true);
    check(hwnd, ID_C_ARCHIVE_SHEET, true);
    check(hwnd, ID_MENU_QUICK, false); // see settings::menu_quick_verbs's documented default
    check(hwnd, ID_MENU_CHECKER, true);
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_APP_THEME) {
        // 0 = follow Windows: the pre-existing behaviour, so "Defaults" restores it.
        SendMessageW(c, CB_SETCURSEL, Some(WPARAM(0)), None);
    }
    check(hwnd, ID_FOLDER_PREBUILD, true); // defaults ON — see settings::folder_prebuild_verb
    check(hwnd, ID_PRESERVE_DATE, false);
    check(hwnd, ID_KEEP_METADATA, true); // losing capture data by accident is the worse default
    check(hwnd, ID_PDF_MARGIN, false); // tight pages are right for scans and comics
    check(hwnd, ID_VERBOSE_LOG, false);
    check(hwnd, ID_UPDATE_AUTO, true); // background update check defaults ON
                                       // Quick preview: reset the behavior toggles to their defaults, but leave the master
                                       // ENABLE alone — like the screenshot service, "Defaults" shouldn't silently kill a
                                       // feature the user turned on.
    check(hwnd, ID_PREVIEW_HOLD_PEEK, true);
    check(hwnd, ID_PREVIEW_CLOSE_FOCUS, false);
    check(hwnd, ID_PREVIEW_TOPMOST, true); // "Open in front" — default ON
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_PREVIEW_BLOCKED_EXTS) {
        let empty = wide(""); // empty by default — see settings::preview_blocked_exts_raw
        let _ = SetWindowTextW(c, PCWSTR(empty.as_ptr()));
    }
    check(hwnd, ID_PREVIEW_TEXT, true);
    check(hwnd, ID_PREVIEW_MARKDOWN, true);
    #[cfg(feature = "html-preview")]
    {
        check(hwnd, ID_PREVIEW_HTML, true); // locked-down (scripts off, no network) → default ON
        check(hwnd, ID_PREVIEW_URL_LIVE, false);
    }
    // Menu preview: reset to the SAME first-run default the getter uses
    // (settings::DEFAULT_MENU_PREVIEW = 1, the SageThumbs submenu). These used to
    // disagree — the getter defaulted to 1 while "Defaults" forced 2 — so a fresh
    // install and pressing "Defaults" produced different menu placement.
    if let Ok(prev) = GetDlgItem(Some(hwnd), ID_MENU_PREVIEW) {
        SendMessageW(
            prev,
            CB_SETCURSEL,
            Some(WPARAM(settings::DEFAULT_MENU_PREVIEW as usize)),
            None,
        );
    }
    // Same discipline as the row above: "Defaults" selects the SAME value the getter falls
    // back to, so a fresh install and pressing Defaults cannot disagree.
    if let Ok(tool) = GetDlgItem(Some(hwnd), ID_SHOT_TOOL) {
        SendMessageW(
            tool,
            CB_SETCURSEL,
            Some(WPARAM(settings::DEFAULT_SHOT_TOOL as usize)),
            None,
        );
    }
    if let Ok(delay) = GetDlgItem(Some(hwnd), ID_SHOT_DELAY) {
        // Index 0 is "Off" — SHOT_DELAY_STEPS[0] is the getter's default, pinned by test.
        SendMessageW(delay, CB_SETCURSEL, Some(WPARAM(0)), None);
    }
    if let Ok(mlist) = GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST) {
        // Factory order + every item shown (rebuilds the rows, dividers included).
        let rows = default_menu_rows(|_| true);
        list::rebuild_rows(mlist, &rows, None);
    }
    // Reset the capture hotkey to its default (Ctrl+PrtScn = first preset). The
    // enable toggle is deliberately left alone — "Defaults" shouldn't silently kill
    // a screenshot service the user turned on.
    if let Ok(shot) = GetDlgItem(Some(hwnd), ID_SHOT_HOTKEY) {
        SendMessageW(shot, CB_SETCURSEL, Some(WPARAM(0)), None);
    }
    // Instant screenshot off by default; reset its combo to the non-colliding
    // default chord and grey it out to match.
    check(hwnd, ID_SHOT_QUICK_ENABLE, false);
    if let Ok(quick) = GetDlgItem(Some(hwnd), ID_SHOT_QUICK_HOTKEY) {
        let d = SHOT_PRESETS
            .iter()
            .position(|&(l, _)| l == QUICK_DEFAULT_LABEL)
            .unwrap_or(0);
        SendMessageW(quick, CB_SETCURSEL, Some(WPARAM(d)), None);
    }
    // Custom action binding: back to the default action (index 0 = colour picker) + unbound.
    if let Ok(act) = GetDlgItem(Some(hwnd), ID_SHOT_ACTION) {
        SendMessageW(act, CB_SETCURSEL, Some(WPARAM(0)), None);
    }
    if let Ok(ahk) = GetDlgItem(Some(hwnd), ID_SHOT_ACTION_HK) {
        SendMessageW(ahk, CB_SETCURSEL, Some(WPARAM(0)), None); // "(none)"
    }
    update_quick_enabled(hwnd);
    check(hwnd, ID_SHOT_HIDE_TRAY, false);
    // Factory reset of the Ctrl+S destination: toggle off + clear the folder (which
    // restores the Desktop default). Clearing the stored dir is written immediately
    // here (like reset_formats), since the folder isn't part of the Save-button apply.
    check(hwnd, ID_SHOT_USE_DIR, false);
    let _ = settings::set_screenshot_save_dir("");
    set_shot_dir_label(hwnd);
    update_save_dir_enabled(hwnd);
    // Remembered Quick preview VIEWER state — the window size you dragged out, the playback
    // volume/mute, and the Markdown outline sidebar. These have no control here (the viewer
    // writes them as you use it), so like the save-folder above they are cleared immediately
    // rather than through the Save-button apply. Without this, "Reset all settings" would leave
    // the viewer opening at a size the user has no other way to undo from this dialog.
    let _ = settings::set_preview_window_size(None);
    let _ = settings::set_preview_volume(100);
    let _ = settings::set_preview_muted(false);
    let _ = settings::set_preview_toc_open(true);
    reset_formats(hwnd); // every supported format re-enabled
                         // Same rule as load_values: dependents are greyed against the FINAL checkbox states.
    sync_dependent_switches(hwnd);
}

/// Reset ONLY the supported-file-types list to its default (every format enabled).
/// Wired to the top-right "Defaults" button — matches its tooltip ("reset the file-type
/// ticks"); the whole-dialog reset is `load_defaults` (the "Reset all settings" button).
pub(in super::super) unsafe fn reset_formats(hwnd: HWND) {
    FMT_STATE.with(|s| {
        for v in s.borrow_mut().iter_mut() {
            *v = true;
        }
    });
    if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
        let count = SendMessageW(list, LVM_GETITEMCOUNT, None, None).0 as i32;
        for i in 0..count {
            set_check(list, i, true);
        }
    }
}

/// Seed the per-format checked model (`FMT_STATE`) from settings: the list view is rebuilt
/// from this model, never from the registry directly, so a search can redraw it without
/// losing toggles. The layout builder, the revert and the refresh all start here.
pub(in super::super) fn seed_format_state() {
    FMT_STATE.with(|s| {
        *s.borrow_mut() = formats::FORMATS
            .iter()
            .map(|&(ext, _)| settings::format_enabled(ext))
            .collect();
    });
}

/// Reload every control from the (just-changed) HKCU settings: the simple controls via
/// [`load_values`], the combo SELECTIONS via [`seed_combo_selections`], plus re-seed the
/// format-list model + repaint it.
pub(in super::super) unsafe fn refresh_from_settings(hwnd: HWND) {
    load_values(hwnd);
    seed_combo_selections(hwnd);
    seed_format_state();
    if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
        // populate_list rebuilds the list UNFILTERED. Without also clearing the search box
        // and its cached needle, retyping the SAME query the box still shows short-circuits
        // on mod.rs's EN_CHANGE equality check (LAST_FILTER == the new needle) and skips the
        // rebuild — leaving the list wrongly unfiltered while the box shows the old text.
        if let Ok(search) = GetDlgItem(Some(hwnd), ID_SEARCH) {
            let empty = wide("");
            let _ = SetWindowTextW(search, PCWSTR(empty.as_ptr()));
        }
        LAST_FILTER.with(|f| *f.borrow_mut() = None);
        populate_list(list, "");
    }
}

/// Combo index for the "default screenshot tool" picker — degrades like
/// `Tool::from_default_index` does, so a hand-edited/out-of-range registry value can't select
/// nothing (a blank combo) or silently disagree with the tool the capture editor actually
/// starts in.
pub(in super::super) fn shot_tool_combo_index(raw: u32) -> u32 {
    if raw < settings::SHOT_TOOL_COUNT {
        raw
    } else {
        settings::DEFAULT_SHOT_TOOL
    }
}

/// Combo index for the capture-delay picker: the position of the stored seconds in
/// [`settings::SHOT_DELAY_STEPS`], degrading to 0 ("Off") for a value the dropdown does not
/// offer — same discipline as [`shot_tool_combo_index`], so a hand-edited registry value can
/// neither blank the combo nor claim a delay the capture will not honour. (The GETTER clamps
/// to 0..=10, so a stored 7 is honoured at capture time but displays as Off — acceptable for
/// a value only reachable by hand-editing the registry.)
pub(in super::super) fn shot_delay_combo_index(raw_secs: u32) -> u32 {
    settings::SHOT_DELAY_STEPS
        .iter()
        .position(|&s| s == raw_secs)
        .unwrap_or(0) as u32
}

/// Combo index matching a packed `(mods << 8 | vk)` hotkey against [`SHOT_PRESETS`], or `0`
/// ("(none)"/first entry) when nothing matches.
pub(in super::super) fn preset_combo_index(packed: u32) -> usize {
    SHOT_PRESETS
        .iter()
        .position(|&(_, p)| p == packed)
        .unwrap_or(0)
}

/// Combo index for the quick-save hotkey: an unbound (`0`) chord falls back to the
/// non-colliding default preset rather than "(none)", matching `build_controls`.
pub(in super::super) fn quick_hotkey_combo_index(packed: u32) -> usize {
    if packed == 0 {
        SHOT_PRESETS
            .iter()
            .position(|&(l, _)| l == QUICK_DEFAULT_LABEL)
            .unwrap_or(0)
    } else {
        preset_combo_index(packed)
    }
}

/// Combo index for the custom-action hotkey: item 0 is "(none)" (unbound), items 1.. mirror
/// [`SHOT_PRESETS`] — so an unbound action (`vk == 0`) is index 0, everything else is offset
/// by one.
pub(in super::super) fn custom_action_hk_combo_index(packed: u32, vk: u32) -> usize {
    if vk == 0 {
        0
    } else {
        SHOT_PRESETS
            .iter()
            .position(|&(_, p)| p == packed)
            .map_or(0, |i| i + 1)
    }
}

/// Re-select every combo whose current index [`load_values`] cannot restore on its own — it
/// has no `CB_SETCURSEL` calls of its own, because these combos are seeded ONCE, inline,
/// when `build::build_controls` creates them. That is fine for the dialog's normal lifetime
/// (the combo keeps whatever the user last picked), but `refresh_from_settings` (post-Import
/// and after an unattended sync pull) calls `load_values` WITHOUT re-running `build_controls`,
/// so without this the combos below kept showing the PRE-import on-screen selection — and
/// `apply_settings` then read that stale index back via `CB_GETCURSEL` and silently overwrote
/// the just-imported value on Save. `ID_LANG` IS included: `apply_settings` reads it on every
/// Save (`settings::set_lang(selected_lang(hwnd)...)`), so leaving it stale here reverted an
/// imported language exactly like the other combos, despite an earlier comment here claiming
/// otherwise. The custom-action Enable checkbox (not a combo, but the same "derived from a
/// setting `load_values` doesn't touch" shape) and its dependent greying are re-derived here
/// too, for the same reason.
pub(in super::super) unsafe fn seed_combo_selections(hwnd: HWND) {
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_APP_THEME) {
        SendMessageW(
            c,
            CB_SETCURSEL,
            Some(WPARAM(settings::app_theme() as usize)),
            None,
        );
    }
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_MENU_PREVIEW) {
        SendMessageW(
            c,
            CB_SETCURSEL,
            Some(WPARAM(settings::menu_preview() as usize)),
            None,
        );
    }
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_SHOT_TOOL) {
        let sel = shot_tool_combo_index(settings::screenshot_default_tool());
        SendMessageW(c, CB_SETCURSEL, Some(WPARAM(sel as usize)), None);
    }
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_SHOT_HOTKEY) {
        let (m, v) = settings::screenshot_hotkey();
        SendMessageW(
            c,
            CB_SETCURSEL,
            Some(WPARAM(preset_combo_index((m << 8) | v))),
            None,
        );
    }
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_SHOT_QUICK_HOTKEY) {
        let (m, v) = settings::screenshot_quick_hotkey();
        SendMessageW(
            c,
            CB_SETCURSEL,
            Some(WPARAM(quick_hotkey_combo_index((m << 8) | v))),
            None,
        );
    }
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_SHOT_ACTION) {
        let cur = settings::custom_action();
        let sel = crate::hotkey::ACTIONS
            .iter()
            .position(|&(id, _)| id == cur)
            .unwrap_or(0);
        SendMessageW(c, CB_SETCURSEL, Some(WPARAM(sel)), None);
    }
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_SHOT_ACTION_HK) {
        let (m, v) = settings::custom_action_hotkey();
        SendMessageW(
            c,
            CB_SETCURSEL,
            Some(WPARAM(custom_action_hk_combo_index((m << 8) | v, v))),
            None,
        );
    }
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_SHOT_DELAY) {
        let sel = shot_delay_combo_index(settings::screenshot_delay_sec());
        SendMessageW(c, CB_SETCURSEL, Some(WPARAM(sel as usize)), None);
    }
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_LANG) {
        let current = settings::lang_override();
        let mut sel = 0i32;
        for (i, code) in lang_codes().iter().enumerate() {
            if current.as_deref() == Some(*code) {
                sel = (i + 1) as i32;
            }
        }
        SendMessageW(c, CB_SETCURSEL, Some(WPARAM(sel as usize)), None);
    }
    // Not a combo, but the same "derived from a setting `load_values` doesn't restore"
    // shape: re-derive the custom-action Enable checkbox from whether a hotkey is bound,
    // then re-grey its two dependent combos to match.
    check(
        hwnd,
        ID_CUSTOM_ACTION_ENABLE,
        settings::custom_action_hotkey().1 != 0,
    );
    update_custom_action_enabled(hwnd);
}
