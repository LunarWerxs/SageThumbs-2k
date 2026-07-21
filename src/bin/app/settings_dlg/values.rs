//! Load/save/apply settings + diagnostics actions (extracted from settings_dlg; parent-hub pattern).

use super::*;

pub(super) unsafe fn load_values(hwnd: HWND) {
    check(hwnd, ID_ENABLE_THUMBS, settings::thumbnails_enabled());
    check(hwnd, ID_USE_EMBEDDED, settings::use_embedded());
    check(hwnd, ID_ENABLE_MENU, settings::menu_enabled());
    check(hwnd, ID_MENU_ALL_TYPES, settings::menu_all_file_types());
    let mb = (settings::max_file_size_bytes() / (1024 * 1024)).min(u32::MAX as u64) as u32;
    let _ = SetDlgItemInt(hwnd, ID_MAXSIZE, mb, false);
    let _ = SetDlgItemInt(hwnd, ID_SIZE, settings::max_thumb_size(), false);
    let _ = SetDlgItemInt(hwnd, ID_JPEG, settings::jpeg_quality() as u32, false);
    let _ = SetDlgItemInt(hwnd, ID_PNG, settings::png_level(), false);
    check(hwnd, ID_C_SORT, settings::container_sort());
    check(hwnd, ID_C_PREFER_COVER, settings::container_prefer_cover());
    check(hwnd, ID_C_SKIP_SCAN, settings::container_skip_scanlation());
    check(hwnd, ID_C_ARCHIVE_SHEET, settings::archive_collage());
    check(hwnd, ID_MENU_QUICK, settings::menu_quick_verbs());
    check(hwnd, ID_MENU_CHECKER, settings::preview_checker());
    check(hwnd, ID_PRESERVE_DATE, settings::preserve_file_date());
    if let Ok(mlist) = GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST) {
        // Rows may be in a custom drag-reorder, so seed each ROW from its own key
        // (via lParam), not by a fixed toggle index.
        let count = SendMessageW(mlist, LVM_GETITEMCOUNT, None, None).0 as i32;
        for row in 0..count {
            if let Some(ti) = menu_row_toggle(mlist, row) {
                set_check(mlist, row, settings::menu_item_shown(MENU_ITEM_TOGGLES[ti].1));
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
    check(hwnd, ID_PREVIEW_CLOSE_FOCUS, settings::preview_close_on_focus_loss());
    check(hwnd, ID_PREVIEW_TOPMOST, settings::preview_open_front());
    check(hwnd, ID_PREVIEW_TEXT, settings::preview_text());
    check(hwnd, ID_PREVIEW_MARKDOWN, settings::preview_markdown());
    check(hwnd, ID_PREVIEW_MD_REMOTE, settings::preview_md_remote_img());
    #[cfg(feature = "html-preview")]
    check(hwnd, ID_PREVIEW_HTML, settings::preview_html());
    #[cfg(feature = "html-preview")]
    check(hwnd, ID_PREVIEW_URL_LIVE, settings::preview_url_live());
    // Instant screenshot is on iff a quick-save hotkey is stored (vk != 0); grey the
    // picker to match.
    check(hwnd, ID_SHOT_QUICK_ENABLE, settings::screenshot_quick_hotkey().1 != 0);
    update_quick_enabled(hwnd);
    refresh_shot_status(hwnd);
    // Seed the Settings-sync row (button label + green "● Synced" badge) from the signed-in
    // state; the background pull (spawn_sync_pull) later refreshes it via WM_APP_SYNC.
    refresh_sync_ui(hwnd);
}

/// Reset every control to the factory defaults (does not write yet).
pub(super) unsafe fn load_defaults(hwnd: HWND) {
    check(hwnd, ID_ENABLE_THUMBS, true);
    check(hwnd, ID_USE_EMBEDDED, true); // ON by default — see settings::use_embedded
    check(hwnd, ID_ENABLE_MENU, true);
    check(hwnd, ID_MENU_ALL_TYPES, false);
    let _ = SetDlgItemInt(hwnd, ID_MAXSIZE, settings::DEFAULT_MAX_FILE_MB, false);
    let _ = SetDlgItemInt(hwnd, ID_SIZE, settings::DEFAULT_THUMB_SIZE, false);
    let _ = SetDlgItemInt(hwnd, ID_JPEG, settings::DEFAULT_JPEG, false);
    let _ = SetDlgItemInt(hwnd, ID_PNG, settings::DEFAULT_PNG, false);
    check(hwnd, ID_C_SORT, true);
    check(hwnd, ID_C_PREFER_COVER, true);
    check(hwnd, ID_C_SKIP_SCAN, false);
    check(hwnd, ID_C_ARCHIVE_SHEET, true);
    check(hwnd, ID_MENU_QUICK, true);
    check(hwnd, ID_MENU_CHECKER, true);
    check(hwnd, ID_PRESERVE_DATE, false);
    check(hwnd, ID_VERBOSE_LOG, false);
    check(hwnd, ID_UPDATE_AUTO, true); // background update check defaults ON
    // Quick preview: reset the behavior toggles to their defaults, but leave the master
    // ENABLE alone — like the screenshot service, "Defaults" shouldn't silently kill a
    // feature the user turned on.
    check(hwnd, ID_PREVIEW_HOLD_PEEK, true);
    check(hwnd, ID_PREVIEW_CLOSE_FOCUS, false);
    check(hwnd, ID_PREVIEW_TOPMOST, true); // "Open in front" — default ON
    check(hwnd, ID_PREVIEW_TEXT, true);
    check(hwnd, ID_PREVIEW_MARKDOWN, true);
    check(hwnd, ID_PREVIEW_MD_REMOTE, false); // outbound fetch from previewed docs → default OFF
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
        SendMessageW(prev, CB_SETCURSEL, Some(WPARAM(settings::DEFAULT_MENU_PREVIEW as usize)), None);
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
        let d = SHOT_PRESETS.iter().position(|&(l, _)| l == QUICK_DEFAULT_LABEL).unwrap_or(0);
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
    reset_formats(hwnd); // every supported format re-enabled
}

/// Reset ONLY the supported-file-types list to its default (every format enabled).
/// Wired to the top-right "Defaults" button — matches its tooltip ("reset the file-type
/// ticks"); the whole-dialog reset is `load_defaults` (the "Reset all settings" button).
pub(super) unsafe fn reset_formats(hwnd: HWND) {
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

/// Enable the quick-save hotkey picker (its label + combo) only while the "instant
/// screenshot" checkbox is on — mirrors how the feature is gated at save time, so
/// the greyed-out combo can't imply an active second hotkey.
pub(super) unsafe fn update_quick_enabled(hwnd: HWND) {
    let on = checked(hwnd, ID_SHOT_QUICK_ENABLE);
    // Disable only the COMBO — it custom-draws a clean grey. The LABEL stays ENABLED
    // (a disabled static renders an etched/blurry look in dark mode) and is greyed via
    // its WM_CTLCOLORSTATIC handler instead; invalidate it so the colour repaints now.
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_SHOT_QUICK_HOTKEY) {
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(c, on);
    }
    if let Ok(lbl) = GetDlgItem(Some(hwnd), ID_LBL_SHOT_QUICK_HK) {
        let _ = InvalidateRect(Some(lbl), None, true);
    }
}

/// Gate the custom-action combos by the "Enable custom action" toggle. When off,
/// force its hotkey combo to "(none)" (so Save writes it unbound) and grey both.
pub(super) unsafe fn update_custom_action_enabled(hwnd: HWND) {
    let on = checked(hwnd, ID_CUSTOM_ACTION_ENABLE);
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_SHOT_ACTION) {
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(c, on);
    }
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_SHOT_ACTION_HK) {
        if !on {
            SendMessageW(c, CB_SETCURSEL, Some(WPARAM(0)), None); // "(none)" — unbound
        }
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(c, on);
    }
}

/// Grey the "Set save folder…" button + the folder display while the "Save to a set
/// folder on Ctrl+S" toggle is OFF (the folder only matters when auto-save is on). The
/// button custom-draws a clean grey when disabled; the display (a static) is dimmed via
/// its WM_CTLCOLORSTATIC handler, so just invalidate it to repaint.
pub(super) unsafe fn update_save_dir_enabled(hwnd: HWND) {
    let on = checked(hwnd, ID_SHOT_USE_DIR);
    if let Ok(b) = GetDlgItem(Some(hwnd), ID_SHOT_SET_DIR) {
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(b, on);
    }
    if let Ok(lbl) = GetDlgItem(Some(hwnd), ID_SHOT_DIR) {
        let _ = InvalidateRect(Some(lbl), None, true);
    }
}

pub(super) unsafe fn banner_rotator(hwnd: HWND) -> Option<(HWND, *mut SponsorRotator)> {
    let banner = GetDlgItem(Some(hwnd), ID_BANNER).ok()?;
    let rot = GetWindowLongPtrW(banner, GWLP_USERDATA) as *mut SponsorRotator;
    (!rot.is_null()).then_some((banner, rot))
}

/// Persist all settings (and re-register formats if the list changed). Apply-only
/// — does NOT close the window, so the user can save and keep tweaking.
pub(super) unsafe fn apply_settings(hwnd: HWND) {
    let _ = settings::set_dword("EnableThumbs", checked(hwnd, ID_ENABLE_THUMBS) as u32);
    let _ = settings::set_dword("UseEmbedded", checked(hwnd, ID_USE_EMBEDDED) as u32);
    let _ = settings::set_dword("EnableMenu", checked(hwnd, ID_ENABLE_MENU) as u32);
    let _ = settings::set_dword("MenuAllFileTypes", checked(hwnd, ID_MENU_ALL_TYPES) as u32);
    let _ = settings::set_dword("MenuQuickVerbs", checked(hwnd, ID_MENU_QUICK) as u32);
    let _ = settings::set_dword("PreviewChecker", checked(hwnd, ID_MENU_CHECKER) as u32);
    let _ = settings::set_dword("PreserveFileDate", checked(hwnd, ID_PRESERVE_DATE) as u32);
    let _ = settings::set_dword("Debug", checked(hwnd, ID_VERBOSE_LOG) as u32);
    let _ = settings::set_update_auto_check(checked(hwnd, ID_UPDATE_AUTO));
    if let Ok(mlist) = GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST) {
        // Persist BOTH per-item visibility AND the row order (drag-to-reorder), reading
        // each row's lParam so a reordered list — items AND divider rows — saves
        // correctly: item rows write their key + checkbox; divider rows write the
        // separator token at their position.
        let count = SendMessageW(mlist, LVM_GETITEMCOUNT, None, None).0 as i32;
        let mut order: Vec<&'static str> = Vec::with_capacity(count as usize);
        for row in 0..count {
            let param = menu_row_param(mlist, row);
            if param == list::SEP_PARAM {
                order.push(MENU_SEP_TOKEN);
            } else if param >= 0 && (param as usize) < MENU_ITEM_TOGGLES.len() {
                let key = MENU_ITEM_TOGGLES[param as usize].1;
                let _ = settings::set_menu_item_shown(key, is_checked(mlist, row));
                order.push(key);
            }
        }
        let _ = settings::set_menu_order(&order);
    }
    if let Ok(prev) = GetDlgItem(Some(hwnd), ID_MENU_PREVIEW) {
        let sel = SendMessageW(prev, CB_GETCURSEL, None, None).0.clamp(0, 2);
        let _ = settings::set_dword("MenuPreview", sel as u32);
    }
    let _ = settings::set_dword("ContainerSort", checked(hwnd, ID_C_SORT) as u32);
    let _ = settings::set_dword("ContainerPreferCover", checked(hwnd, ID_C_PREFER_COVER) as u32);
    let _ = settings::set_dword("ContainerSkipScanlation", checked(hwnd, ID_C_SKIP_SCAN) as u32);
    let _ = settings::set_dword("ArchiveCollage", checked(hwnd, ID_C_ARCHIVE_SHEET) as u32);

    let mut ok = Default::default();
    let max_mb = GetDlgItemInt(hwnd, ID_MAXSIZE, Some(&mut ok), false);
    let _ = settings::set_dword("MaxSize", if ok.as_bool() { max_mb } else { settings::DEFAULT_MAX_FILE_MB });

    let size = GetDlgItemInt(hwnd, ID_SIZE, Some(&mut ok), false);
    let size = if ok.as_bool() {
        size.clamp(settings::THUMB_MIN, settings::THUMB_MAX)
    } else {
        settings::DEFAULT_THUMB_SIZE
    };
    let _ = settings::set_dword("Width", size);
    let _ = settings::set_dword("Height", size);

    let jpeg = GetDlgItemInt(hwnd, ID_JPEG, Some(&mut ok), false).min(100);
    let _ = settings::set_dword("JPEG", if ok.as_bool() { jpeg } else { settings::DEFAULT_JPEG });
    let png = GetDlgItemInt(hwnd, ID_PNG, Some(&mut ok), false).min(9);
    let _ = settings::set_dword("PNG", if ok.as_bool() { png } else { settings::DEFAULT_PNG });

    // Persist the UI-language choice ("" = follow the system language).
    let _ = settings::set_lang(selected_lang(hwnd).unwrap_or(""));

    // Screenshot capture service: persist the chosen hotkey, then enable/disable the
    // daemon (HKCU autostart + the running tray helper). If it stays enabled and a
    // daemon is already running with a different chord, nudge it to re-register.
    if let Ok(shot) = GetDlgItem(Some(hwnd), ID_SHOT_HOTKEY) {
        let sel = SendMessageW(shot, CB_GETCURSEL, None, None).0;
        if sel >= 0 {
            if let Some(&(_, packed)) = SHOT_PRESETS.get(sel as usize) {
                let _ = settings::set_screenshot_hotkey(packed);
            }
        }
    }
    // Instant screenshot: the checkbox is the on/off switch. On → save the combo's
    // chord; off → save 0 so the daemon skips registering a second hotkey.
    let quick_on = checked(hwnd, ID_SHOT_QUICK_ENABLE);
    let qpacked = if !quick_on {
        0
    } else if let Ok(quick) = GetDlgItem(Some(hwnd), ID_SHOT_QUICK_HOTKEY) {
        let qsel = SendMessageW(quick, CB_GETCURSEL, None, None).0;
        SHOT_PRESETS.get(qsel.max(0) as usize).map_or(0, |&(_, p)| p)
    } else {
        0
    };
    let _ = settings::set_screenshot_quick_hotkey(qpacked);
    let _ = settings::set_dword("ScreenshotHideTray", checked(hwnd, ID_SHOT_HIDE_TRAY) as u32);
    let _ = settings::set_screenshot_use_save_dir(checked(hwnd, ID_SHOT_USE_DIR));
    // Custom action hotkey: persist the chosen action + its chord (item 0 of the hotkey combo
    // = "(none)" = unbound). Written BEFORE set_enabled() below so the daemon reconcile — which
    // keeps the daemon resident whenever a custom hotkey is bound — sees the new state.
    if let Ok(act) = GetDlgItem(Some(hwnd), ID_SHOT_ACTION) {
        let sel = SendMessageW(act, CB_GETCURSEL, None, None).0;
        if let Some(&(id, _)) = crate::hotkey::ACTIONS.get(sel.max(0) as usize) {
            let _ = settings::set_custom_action(id);
        }
    }
    if let Ok(ahk) = GetDlgItem(Some(hwnd), ID_SHOT_ACTION_HK) {
        let sel = SendMessageW(ahk, CB_GETCURSEL, None, None).0;
        let packed = if sel <= 0 {
            0 // "(none)" — unbound
        } else {
            SHOT_PRESETS.get((sel - 1) as usize).map_or(0, |&(_, p)| p)
        };
        let _ = settings::set_custom_action_hotkey(packed);
    }
    // Quick preview: persist the master toggle + behavior prefs. Written BEFORE
    // set_enabled() below so the daemon reconcile — which keeps the daemon resident
    // whenever Quick preview is enabled (via daemon_wanted) — sees the new state and
    // starts/stops the daemon + autostart entry to match.
    let _ = settings::set_preview_enabled(checked(hwnd, ID_PREVIEW_ENABLED));
    let _ = settings::set_preview_hold_peek(checked(hwnd, ID_PREVIEW_HOLD_PEEK));
    let _ = settings::set_preview_close_on_focus_loss(checked(hwnd, ID_PREVIEW_CLOSE_FOCUS));
    let _ = settings::set_preview_open_front(checked(hwnd, ID_PREVIEW_TOPMOST));
    let _ = settings::set_preview_text(checked(hwnd, ID_PREVIEW_TEXT));
    let _ = settings::set_preview_markdown(checked(hwnd, ID_PREVIEW_MARKDOWN));
    let _ = settings::set_preview_md_remote_img(checked(hwnd, ID_PREVIEW_MD_REMOTE));
    #[cfg(feature = "html-preview")]
    {
        let _ = settings::set_preview_html(checked(hwnd, ID_PREVIEW_HTML));
        let _ = settings::set_preview_url_live(checked(hwnd, ID_PREVIEW_URL_LIVE));
    }

    let shot_on = checked(hwnd, ID_SHOT_ENABLE);
    // set_enabled persists the screenshot flag, then reconciles the daemon (start/stop +
    // re-register) accounting for the screenshot feature, the custom hotkey saved above,
    // AND Quick preview persisted just above — so it covers the "daemon needed only for a
    // custom hotkey / Quick preview" cases too.
    crate::screenshot::set_enabled(shot_on);

    // Per-format flags. Collect the changes first; persist them, then run the
    // elevated re-register that rewrites the HKCR shell hooks to match. If that
    // elevation is declined or fails, roll the HKCU flags back so the persisted
    // settings stay consistent with the (unchanged) hooks — otherwise the two
    // silently diverge and, because change-detection reads HKCU, never reconcile.
    // Save from the model (the list may be filtered, so its rows are a subset).
    let mut changes: Vec<(&'static str, bool, bool)> = Vec::new();
    FMT_STATE.with(|st| {
        let st = st.borrow();
        for (i, &(ext, _)) in formats::FORMATS.iter().enumerate() {
            let want = st.get(i).copied().unwrap_or_else(|| settings::format_enabled(ext));
            let old = settings::format_enabled(ext);
            if old != want {
                changes.push((ext, want, old));
            }
        }
    });
    if !changes.is_empty() {
        for &(ext, want, _) in &changes {
            let _ = settings::set_format_enabled(ext, want);
        }
        // Only Ok counts — any other outcome means the HKCR hooks do NOT match the flags
        // we just wrote, so roll the flags back rather than leave the UI lying.
        if !matches!(reregister_elevated(), Reg::Ok) {
            for &(ext, _, old) in &changes {
                let _ = settings::set_format_enabled(ext, old);
            }
            message_box(hwnd, t("msg_admin_required"), "SageThumbs 2K");
        }
    }

    // Nudge the shell to drop its cached file-association / context-menu state so a
    // menu toggle (e.g. MenuQuickVerbs, per-item visibility, the reorder) takes
    // effect on the NEXT right-click instead of silently waiting for an Explorer
    // restart. The classic IContextMenu handler reads settings live, so this flushes
    // the shell's association cache around it; the modern packaged verbs re-query
    // GetState per menu-build, so they pick the change up on the next open too.
    notify_shell_assoc_changed();
}

/// `SHChangeNotify(SHCNE_ASSOCCHANGED)` — tells Explorer file-type handlers changed,
/// so it re-reads context-menu registrations rather than serving a stale cache. The
/// standard post-settings nudge for a shell extension; cheap and side-effect-free.
fn notify_shell_assoc_changed() {
    use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

/// Export the saved settings to a user-chosen `.json` file (Diagnostics ▸ Export).
pub(super) unsafe fn export_settings_to_file(hwnd: HWND) {
    let Some(path) = crate::win::pick_save_settings(hwnd, "SageThumbs2K-settings.json") else {
        return;
    };
    match std::fs::write(&path, crate::settings_io::export_settings()) {
        Ok(()) => msg(hwnd, &format!("Settings exported to:\n{path}"), "Export Settings", MB_ICONINFORMATION),
        Err(e) => msg(hwnd, &format!("Couldn't write the file:\n\n{e}"), "Export Settings", MB_ICONERROR),
    }
}

/// Import settings from a user-chosen `.json` file: apply them to HKCU, refresh the
/// dialog, and re-register the machine-wide shell hooks if the per-format enables
/// changed (Diagnostics ▸ Import).
pub(super) unsafe fn import_settings_from_file(hwnd: HWND) {
    let Some(path) = crate::win::pick_open_settings(hwnd) else {
        return;
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return msg(hwnd, &format!("Couldn't read the file:\n\n{e}"), "Import Settings", MB_ICONERROR),
    };
    // Snapshot the per-format enables so we only trigger the (elevated) re-register when
    // the import actually changed which formats are hooked.
    let before: Vec<bool> = formats::FORMATS.iter().map(|&(ext, _)| settings::format_enabled(ext)).collect();
    match crate::settings_io::import_settings(&text) {
        Err(e) => msg(hwnd, &e, "Import Settings", MB_ICONERROR),
        Ok(n) => {
            refresh_from_settings(hwnd);
            let formats_changed = formats::FORMATS
                .iter()
                .enumerate()
                .any(|(i, &(ext, _))| settings::format_enabled(ext) != before[i]);
            if formats_changed {
                // Sync the machine-wide HKCR hooks to the imported per-format flags.
                let _ = reregister_elevated();
            }
            msg(hwnd, &format!("Imported {n} settings — applied now."), "Import Settings", MB_ICONINFORMATION);
        }
    }
}

/// Reload every control from the (just-changed) HKCU settings: the simple controls via
/// [`load_values`], plus re-seed the format-list model + repaint it.
pub(super) unsafe fn refresh_from_settings(hwnd: HWND) {
    load_values(hwnd);
    FMT_STATE.with(|s| {
        *s.borrow_mut() = formats::FORMATS.iter().map(|&(ext, _)| settings::format_enabled(ext)).collect();
    });
    if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
        populate_list(list, "");
    }
}

/// An OK message box with an explicit info/error icon, for the import/export feedback.
/// (`win::message_box` is warning-only.)
pub(super) unsafe fn msg(hwnd: HWND, text: &str, caption: &str, icon: MESSAGEBOX_STYLE) {
    let t = wide(text);
    let c = wide(caption);
    MessageBoxW(Some(hwnd), PCWSTR(t.as_ptr()), PCWSTR(c.as_ptr()), MB_OK | icon);
}

/// Clear Windows' thumbnail cache and restart Explorer so thumbnails rebuild. Per-user,
/// no elevation needed (the cache lives in the user's own LocalAppData). Behind a confirm
/// — it briefly blinks the taskbar. This is the fix for the classic "I changed a setting
/// but the thumbnails look the same" (Explorer keeps serving stale cached thumbnails).
pub(super) unsafe fn rebuild_thumbnail_cache(hwnd: HWND) {
    let warn = wide(
        "This clears Windows' thumbnail cache and briefly restarts File Explorer (your \
         taskbar will blink). Open windows and files are not affected.\n\nContinue?",
    );
    let cap = wide("Rebuild Thumbnail Cache");
    if MessageBoxW(Some(hwnd), PCWSTR(warn.as_ptr()), PCWSTR(cap.as_ptr()), MB_YESNO | MB_ICONWARNING) != IDYES {
        return;
    }
    // Kill Explorer (releases the cache files' lock), delete thumbcache_*.db, relaunch.
    // Must go through `shellcmd::cmd_c` — `Command::args` would escape the quotes for
    // the MSVCRT convention and `cmd` would misread them (see shellcmd, issue #5).
    let _ = sagethumbs2k_core::shellcmd::cmd_c(sagethumbs2k_core::shellcmd::RESTART_EXPLORER_CLEARING_CACHE);
    msg(
        hwnd,
        "Thumbnail cache cleared and Explorer restarted. Thumbnails will rebuild as you browse.",
        "Rebuild Thumbnail Cache",
        MB_ICONINFORMATION,
    );
}

/// Open the diagnostics log in the user's default text editor (or its folder if the
/// log doesn't exist yet), so a user can find it and send it in for a bug report.
pub(super) unsafe fn open_diagnostics_log() {
    let path = match sagethumbs2k_core::safety::log_file() {
        Some(p) if p.exists() => p,
        // No log yet → open its folder (the user sees there's nothing to send).
        Some(p) => p.parent().map(|d| d.to_path_buf()).unwrap_or(p),
        None => return,
    };
    let file = wide(&path.display().to_string());
    let verb = wide("open");
    ShellExecuteW(
        Some(HWND::default()),
        PCWSTR(verb.as_ptr()),
        PCWSTR(file.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}

/// Why a re-registration attempt ended the way it did. The distinction matters to the
/// user: "you declined the prompt" and "your antivirus ate the DLL" need opposite
/// actions, and both used to surface as the same cheerful success message.
pub(super) enum Reg {
    Ok,
    /// The DLL is not on disk — the usual cause is security software quarantining it.
    MissingDll,
    /// `ShellExecute` could not start `regsvr32` (typically a declined UAC prompt).
    NotLaunched,
    /// `regsvr32` ran and reported failure.
    Failed(u32),
    /// `regsvr32` reported success but the CLSID still is not there.
    NotRegistered,
}

/// Re-run `regsvr32` elevated against the installed DLL, and **verify it worked**.
/// `register()` reads the per-extension flags we just wrote, so this brings the HKCR
/// `shellex` keys in line with the Options format list.
///
/// The old version returned success as soon as `ShellExecute` *launched* regsvr32 —
/// which says nothing about whether registration happened. A user whose DLL had been
/// quarantined got "File associations repaired." and still had no thumbnails. So now we
/// wait for the process, check its exit code, and then read the CLSID back.
pub(super) unsafe fn reregister_elevated() -> Reg {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};

    let dll = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("sagethumbs2k.dll")))
        .unwrap_or_default();
    if !dll.exists() {
        return Reg::MissingDll;
    }

    let params = wide(&format!("/s \"{}\"", dll.display()));
    let verb = wide("runas");
    let file = wide("regsvr32.exe");

    // ShellExecuteExW (not ShellExecuteW) — it is the only variant that hands back a
    // process handle, which is what lets us wait for an answer instead of assuming one.
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    if ShellExecuteExW(&mut info).is_err() || info.hProcess.is_invalid() {
        return Reg::NotLaunched;
    }

    // regsvr32 is a fast, local registry write; 60 s is far past any legitimate run and
    // still bounded, so a wedged process can't hang the Settings window forever.
    let outcome = if WaitForSingleObject(info.hProcess, 60_000) == WAIT_OBJECT_0 {
        let mut code = 0u32;
        if GetExitCodeProcess(info.hProcess, &mut code).is_ok() && code != 0 {
            Reg::Failed(code)
        } else {
            Reg::Ok
        }
    } else {
        Reg::Failed(u32::MAX) // timed out — treat as failure rather than guess success
    };
    let _ = CloseHandle(info.hProcess);

    // Even a zero exit code gets checked against reality: this is the condition that
    // actually matters, and it is cheap to confirm.
    match outcome {
        Reg::Ok if !sagethumbs2k_core::register::is_registered() => Reg::NotRegistered,
        other => other,
    }
}

/// "Repair file associations" — the fix for blank/stuck thumbnails after another program
/// stole SageThumbs' shell hooks (the classic complaint), or an update left them stale.
/// Re-runs the full elevated registration (rewrites every enabled format's thumbnail /
/// context-menu / property hooks back to us), then clears the thumbnail cache + restarts
/// Explorer so the repaired thumbnails render immediately instead of serving stale blanks.
pub(super) unsafe fn repair_associations(hwnd: HWND) {
    let warn = wide(
        "This re-registers SageThumbs 2K for all your enabled file types — the fix when \
         thumbnails go blank after another program takes over a format — then clears the \
         thumbnail cache and briefly restarts File Explorer (your taskbar will blink).\n\nContinue?",
    );
    let cap = wide("Repair File Associations");
    if MessageBoxW(Some(hwnd), PCWSTR(warn.as_ptr()), PCWSTR(cap.as_ptr()), MB_YESNO | MB_ICONWARNING) != IDYES {
        return;
    }
    // Report what actually happened. Each of these needs a different action from the
    // user, so collapsing them into one message is what made this button useless as a
    // diagnostic in the first place.
    match reregister_elevated() {
        Reg::Ok => {}
        Reg::MissingDll => {
            return msg(
                hwnd,
                "sagethumbs2k.dll is missing from the install folder, so there is nothing to \
                 register.\n\nThis is almost always security software quarantining it. Allow \
                 the SageThumbs 2K folder in your antivirus, then reinstall.",
                "Repair File Associations",
                MB_ICONERROR,
            )
        }
        Reg::NotLaunched => {
            return msg(
                hwnd,
                "Couldn't start regsvr32 — the elevation prompt was declined or failed. \
                 Nothing was changed.",
                "Repair File Associations",
                MB_ICONERROR,
            )
        }
        Reg::Failed(code) => {
            return msg(
                hwnd,
                &format!(
                    "regsvr32 could not register the shell extension (error {code}). \
                     Nothing was changed.\n\nRun 'st2k doctor' from the install folder and \
                     include its output in a bug report."
                ),
                "Repair File Associations",
                MB_ICONERROR,
            )
        }
        Reg::NotRegistered => {
            return msg(
                hwnd,
                "regsvr32 reported success, but the shell extension is still not registered.\
                 \n\nSomething is undoing the registration — usually security software. Run \
                 'st2k doctor' from the install folder and include its output in a bug report.",
                "Repair File Associations",
                MB_ICONERROR,
            )
        }
    }
    // Registration rewrote the hooks; drop the stale cached thumbnails + restart Explorer so
    // the repaired ones render right away. (The cmd sequence gives regsvr32 time to finish.)
    let _ = sagethumbs2k_core::shellcmd::cmd_c(sagethumbs2k_core::shellcmd::RESTART_EXPLORER_CLEARING_CACHE);
    msg(
        hwnd,
        "File associations repaired. Thumbnails will rebuild as you browse.",
        "Repair File Associations",
        MB_ICONINFORMATION,
    );
}

