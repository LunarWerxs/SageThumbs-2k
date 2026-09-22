//! Controls into settings: the Apply chain, one function per page, every write tracked for failure.

use super::*;

/// Every `settings::set_*` call in the `apply_*` chain below is wrapped in this instead of
/// the bare `let _ = ...;` the whole file used to write everywhere — with the exception of
/// the two `set_format_enabled` writes in `apply_format_flags`: a failed HKCU write (a
/// portable ini on read-only media, a sharing violation, permissions) used to vanish with no
/// error and no log line — the dialog closed clean, the file/registry never changed, and
/// there was nothing anywhere to say why. This just records that SOMETHING failed; the
/// caller decides what to do with it. Passes the result through unchanged.
pub(super) fn note<T, E>(r: Result<T, E>) -> Result<T, E> {
    if r.is_err() {
        SAVE_FAILED.with(|f| f.set(true));
    }
    r
}

/// Persist all settings (and re-register formats if the list changed). Apply-only
/// — does NOT close the window, so the user can save and keep tweaking.
pub(in super::super) unsafe fn apply_settings(hwnd: HWND) {
    SAVE_FAILED.with(|f| f.set(false));
    let badge_changed = apply_thumbnail_and_badge_settings(hwnd);
    apply_menu_and_misc_toggles(hwnd);
    apply_menu_item_list_order(hwnd);
    apply_menu_preview_and_theme(hwnd);
    apply_screenshot_tool_prefs(hwnd);
    apply_container_settings(hwnd);
    apply_tuning_numbers(hwnd);
    let _ = note(settings::set_lang(selected_lang(hwnd).unwrap_or("")));
    apply_screenshot_hotkeys(hwnd);
    apply_quick_preview_and_screenshot_enable(hwnd);
    apply_format_flags(hwnd);
    // One message covers every tracked write above — apply_format_flags' own elevation
    // failure already shows its own (more specific, "admin required") message, so it is
    // deliberately not routed through `note`.
    if SAVE_FAILED.with(|f| f.get()) {
        let where_ = settings::ini_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| t("save_failed_registry").to_string());
        message_box(
            hwnd,
            &t("msg_save_failed").replace("{path}", &where_),
            "SageThumbs 2K",
        );
    }

    // Nudge the shell to drop its cached file-association / context-menu state so a
    // menu toggle (e.g. MenuQuickVerbs, per-item visibility, the reorder) takes
    // effect on the NEXT right-click instead of silently waiting for an Explorer
    // restart. The classic IContextMenu handler reads settings live, so this flushes
    // the shell's association cache around it; the modern packaged verbs re-query
    // GetState per menu-build, so they pick the change up on the next open too.
    notify_shell_assoc_changed();

    // The badge lives INSIDE the cached bitmap, so a toggle is invisible until the shell's
    // thumbnail cache is discarded. Do it for the user, and only when the value actually
    // changed - an unrelated Apply must never blow away everyone's cached tiles. Backgrounded
    // (see `spawn_cache_rebuild`) — this used to block Apply's caller for up to ~33s with no
    // feedback, the exact freeze the explicit "Rebuild Thumbnail Cache" button already avoided.
    if badge_changed {
        spawn_cache_rebuild(hwnd, None);
    }
}

/// The thumbnail-generation + format-badge settings, returning whether anything baked into
/// the cached bitmap changed (the caller purges the shell's thumbnail cache only then).
pub(super) unsafe fn apply_thumbnail_and_badge_settings(hwnd: HWND) -> bool {
    let _ = note(settings::set_dword(
        "EnableThumbs",
        checked(hwnd, ID_ENABLE_THUMBS) as u32,
    ));
    let _ = note(settings::set_dword(
        "UseEmbedded",
        checked(hwnd, ID_USE_EMBEDDED) as u32,
    ));
    // The format badge is baked INTO the bitmap the shell caches, so flipping it changes
    // nothing the user can see until the thumbnail cache is discarded. Detect the change
    // here and purge, otherwise the first thing every user reports is "I ticked it and
    // nothing happened". Only on an actual change — never make Apply nuke the cache.
    let mark_now = settings::CornerMark::from_dword(combo_sel(hwnd, ID_CORNER_MARK, 2) as u32);
    let icon_now = checked(hwnd, ID_BADGE_ICON);
    // Same rule as the style switch: the SIZE is baked into the cached bitmap too, so a change
    // that skipped the purge would look like it did nothing.
    let size_now = settings::BadgeSize::from_dword(combo_sel(hwnd, ID_BADGE_SIZE, 2) as u32);
    let checker_now = checked(hwnd, ID_THUMB_CHECKER);
    // Cover-art-versus-frame decides WHICH PICTURE the tile is, so it belongs to this set
    // too: without the purge, ticking it looks like it did nothing until the cache happens
    // to turn over.
    let cover_now = checked(hwnd, ID_VIDEO_COVER_ART);
    // Every one of these is baked into the cached bitmap, so they share the badge's
    // purge-on-change rule. Compute the OR before writing, or the comparison reads back
    // the value we just stored and never fires.
    let mark_was = settings::corner_mark();
    let overlay_was = settings::hide_type_overlay();
    let badge_changed = mark_now != mark_was
        || icon_now != settings::format_badge_icon()
        || size_now != settings::badge_size()
        || checker_now != settings::thumb_checker()
        || cover_now != settings::prefer_cover_art();
    let _ = note(settings::set_corner_mark(mark_now));
    let _ = note(settings::set_format_badge_icon(icon_now));
    let _ = note(settings::set_badge_size(size_now));
    let _ = note(settings::set_thumb_checker(checker_now));
    let _ = note(settings::set_prefer_cover_art(cover_now));
    // The corner mark's OTHER half. Explorer draws its own overlay on top of what it cached,
    // so suppressing it is not a bitmap change and needs no purge — but it DOES need the
    // per-ProgID registry written and the shell told, which `typeoverlay::sync` does for every
    // hooked format. Read AFTER the write, so this compares against the value that now stands.
    let overlay_now = settings::hide_type_overlay();
    if overlay_now != overlay_was {
        sagethumbs2k_core::typeoverlay::sync(overlay_now);
    }
    badge_changed
}

/// The remaining simple flag/dword toggles: folder prebuild verb, menu enable flags,
/// file-date/metadata preservation, PDF layout, verbose logging, and the update-check
/// schedule.
pub(super) unsafe fn apply_menu_and_misc_toggles(hwnd: HWND) {
    // Same shape as the overlay above: a registry-only change the shell reads directly, so it
    // needs writing and syncing but no thumbnail purge.
    let folder_verb_now = checked(hwnd, ID_FOLDER_PREBUILD);
    if folder_verb_now != settings::folder_prebuild_verb() {
        let _ = note(settings::set_folder_prebuild_verb(folder_verb_now));
        sagethumbs2k_core::foldermenu::sync(folder_verb_now);
    }
    let _ = note(settings::set_dword(
        "EnableMenu",
        checked(hwnd, ID_ENABLE_MENU) as u32,
    ));
    let _ = note(settings::set_dword(
        "MenuAllFileTypes",
        checked(hwnd, ID_MENU_ALL_TYPES) as u32,
    ));
    let _ = note(settings::set_dword(
        "MenuQuickVerbs",
        checked(hwnd, ID_MENU_QUICK) as u32,
    ));
    let _ = note(settings::set_dword(
        "PreviewChecker",
        checked(hwnd, ID_MENU_CHECKER) as u32,
    ));
    let _ = note(settings::set_dword(
        "PreserveFileDate",
        checked(hwnd, ID_PRESERVE_DATE) as u32,
    ));
    let _ = note(settings::set_dword(
        "KeepMetadata",
        checked(hwnd, ID_KEEP_METADATA) as u32,
    ));
    // Only ever toggles between tight (0) and margin (1); a registry-chosen sheet mode
    // (2/3) is left alone unless the user actually unticks the box.
    if checked(hwnd, ID_PDF_MARGIN) {
        if settings::pdf_page() == sagethumbs2k_core::PdfPage::Tight {
            let _ = note(settings::set_dword("PdfLayout", 1));
        }
    } else {
        let _ = note(settings::set_dword("PdfLayout", 0));
    }
    let _ = note(settings::set_dword(
        "Debug",
        checked(hwnd, ID_VERBOSE_LOG) as u32,
    ));
    let _ = note(settings::set_update_auto_check(checked(
        hwnd,
        ID_UPDATE_AUTO,
    )));
    // The periodic check is a per-user Scheduled Task, not a resident process, so the
    // toggle has to create/remove that task — otherwise turning it OFF would leave the
    // task running (and turning it back ON after an install where task creation failed
    // would never bring it back). Best-effort: the launch-time piggyback covers either way.
    crate::update::sync_update_task();
}

/// Persist the context-menu item list's per-item visibility AND row order.
pub(super) unsafe fn apply_menu_item_list_order(hwnd: HWND) {
    let Ok(mlist) = GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST) else {
        return;
    };
    // Persist BOTH per-item visibility AND the row order (drag-to-reorder), reading
    // each row's lParam so a reordered list - items AND divider rows - saves
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
            let _ = note(settings::set_menu_item_shown(key, is_checked(mlist, row)));
            order.push(key);
        }
    }
    let _ = note(settings::set_menu_order(&order));
}

/// The preview-mode combo (Explorer icon/classic/menu-preview toggle) and the app theme
/// combo. The theme resolves once per process, so a running Quick preview can't repaint
/// itself in a new skin - retire it on an actual change so the next Space press opens a
/// fresh one already wearing it.
pub(super) unsafe fn apply_menu_preview_and_theme(hwnd: HWND) {
    if let Ok(prev) = GetDlgItem(Some(hwnd), ID_MENU_PREVIEW) {
        let sel = SendMessageW(prev, CB_GETCURSEL, None, None).0.clamp(0, 2);
        let _ = note(settings::set_dword("MenuPreview", sel as u32));
    }
    let theme_before = settings::app_theme();
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_APP_THEME) {
        // CB_ERR is -1 (nothing selected); clamp instead of storing it.
        let sel = SendMessageW(c, CB_GETCURSEL, None, None).0.clamp(0, 2);
        let _ = note(settings::set_app_theme(sel as u32));
        if sel as u32 != theme_before {
            crate::preview::request_close();
        }
    }
}

/// Screenshot default tool + capture delay combos.
pub(super) unsafe fn apply_screenshot_tool_prefs(hwnd: HWND) {
    if let Ok(tool) = GetDlgItem(Some(hwnd), ID_SHOT_TOOL) {
        // CB_ERR is -1 (no selection); clamp to a real index rather than storing it.
        let sel = SendMessageW(tool, CB_GETCURSEL, None, None).0.max(0);
        let _ = note(settings::set_screenshot_default_tool(sel as u32));
    }
    if let Ok(delay) = GetDlgItem(Some(hwnd), ID_SHOT_DELAY) {
        let sel = SendMessageW(delay, CB_GETCURSEL, None, None).0.max(0) as usize;
        let secs = settings::SHOT_DELAY_STEPS
            .get(sel)
            .copied()
            .unwrap_or_default();
        let _ = note(settings::set_screenshot_delay_sec(secs));
    }
}

/// The four container-format checkboxes (sort, prefer cover, skip scanlation, archive
/// contact sheet).
pub(super) unsafe fn apply_container_settings(hwnd: HWND) {
    let _ = note(settings::set_dword(
        "ContainerSort",
        checked(hwnd, ID_C_SORT) as u32,
    ));
    let _ = note(settings::set_dword(
        "ContainerPreferCover",
        checked(hwnd, ID_C_PREFER_COVER) as u32,
    ));
    let _ = note(settings::set_dword(
        "ContainerSkipScanlation",
        checked(hwnd, ID_C_SKIP_SCAN) as u32,
    ));
    let _ = note(settings::set_dword(
        "ArchiveCollage",
        checked(hwnd, ID_C_ARCHIVE_SHEET) as u32,
    ));
}

/// The numeric tuning fields (MaxSize, thumbnail size, video offset).
///
/// These go through `set_dword_tracking_default`, which stores a value only when it
/// differs from the default and DELETES it when it matches. This dialog writes every
/// setting on every OK whether or not it was touched, so a plain `set_dword` here would
/// pin each value at whatever the default was on the day the user first clicked OK - and
/// no later default change could reach them. `MaxSize` is why that matters: it shipped
/// defaulting to exactly the engine's buffering ceiling, which made the oversized-file
/// rescue unreachable, and the repair is a raised DEFAULT that only lands on users whose
/// value is absent. See `settings::set_dword_tracking_default`.
///
/// Deliberately NOT applied to the checkboxes on this page. Their defaults are product
/// decisions that do not get retuned, and each polarity would have to be re-derived by hand
/// from its accessor - getting one wrong silently INVERTS a setting for every user who ever
/// pressed OK, which is a far worse failure than the one being fixed.
pub(in super::super) unsafe fn apply_tuning_numbers(hwnd: HWND) {
    let mut ok = Default::default();
    let max_mb = GetDlgItemInt(hwnd, ID_MAXSIZE, Some(&mut ok), false);
    let _ = note(settings::set_dword_tracking_default(
        "MaxSize",
        if ok.as_bool() {
            max_mb
        } else {
            settings::DEFAULT_MAX_FILE_MB
        },
        settings::DEFAULT_MAX_FILE_MB,
    ));

    let size = GetDlgItemInt(hwnd, ID_SIZE, Some(&mut ok), false);
    let size = if ok.as_bool() {
        size.clamp(settings::THUMB_MIN, settings::THUMB_MAX)
    } else {
        settings::DEFAULT_THUMB_SIZE
    };
    let _ = note(settings::set_dword_tracking_default(
        "Width",
        size,
        settings::DEFAULT_THUMB_SIZE,
    ));
    let _ = note(settings::set_dword_tracking_default(
        "Height",
        size,
        settings::DEFAULT_THUMB_SIZE,
    ));

    // How far into a video the thumbnail frame comes from (issue #26.4). An empty or
    // non-numeric box restores the default rather than silently meaning 0 %, which would give
    // the first frame — the black one people are trying to get AWAY from.
    let offset = GetDlgItemInt(hwnd, ID_VIDEO_OFFSET, Some(&mut ok), false);
    let _ = note(settings::set_video_offset_pct(if ok.as_bool() {
        offset
    } else {
        settings::DEFAULT_VIDEO_OFFSET_PCT
    }));

    let jpeg = GetDlgItemInt(hwnd, ID_JPEG, Some(&mut ok), false).clamp(1, 100);
    let _ = note(settings::set_dword(
        "JPEG",
        if ok.as_bool() {
            jpeg
        } else {
            settings::DEFAULT_JPEG
        },
    ));
    let png = GetDlgItemInt(hwnd, ID_PNG, Some(&mut ok), false).min(9);
    let _ = note(settings::set_dword(
        "PNG",
        if ok.as_bool() {
            png
        } else {
            settings::DEFAULT_PNG
        },
    ));
}

/// Screenshot capture service: hotkey, quick hotkey, hide-tray, save-dir, and the
/// user-assignable custom-action hotkey. Written BEFORE `set_enabled()` runs (see
/// [`apply_quick_preview_and_screenshot_enable`]) so the daemon reconcile sees the new
/// state.
pub(super) unsafe fn apply_screenshot_hotkeys(hwnd: HWND) {
    // Read the packed chord back via CB_GETITEMDATA, not by re-deriving it from
    // the selected index into SHOT_PRESETS: `build_controls` stashes the real
    // packed value on every item, including the trailing "unknown chord" item it
    // appends for a stored value outside the curated list, so this round-trips
    // that value instead of silently collapsing it to preset 0.
    if let Ok(shot) = GetDlgItem(Some(hwnd), ID_SHOT_HOTKEY) {
        let sel = SendMessageW(shot, CB_GETCURSEL, None, None).0;
        if sel >= 0 {
            let packed =
                SendMessageW(shot, CB_GETITEMDATA, Some(WPARAM(sel as usize)), None).0 as u32;
            let _ = note(settings::set_screenshot_hotkey(packed));
        }
    }
    // Instant screenshot: the checkbox is the on/off switch. On → save the combo's
    // chord; off → save 0 so the daemon skips registering a second hotkey.
    let qpacked = quick_shot_chord(hwnd);
    let _ = note(settings::set_screenshot_quick_hotkey(qpacked));
    let _ = note(settings::set_dword(
        "ScreenshotHideTray",
        checked(hwnd, ID_SHOT_HIDE_TRAY) as u32,
    ));
    let _ = note(settings::set_screenshot_use_save_dir(checked(
        hwnd,
        ID_SHOT_USE_DIR,
    )));
    // Custom action hotkey: persist the chosen action + its chord (item 0 of the hotkey combo
    // = "(none)" = unbound). Written BEFORE set_enabled() below so the daemon reconcile — which
    // keeps the daemon resident whenever a custom hotkey is bound — sees the new state.
    if let Ok(act) = GetDlgItem(Some(hwnd), ID_SHOT_ACTION) {
        let sel = SendMessageW(act, CB_GETCURSEL, None, None).0;
        if let Some(&(id, _)) = crate::hotkey::ACTIONS.get(sel.max(0) as usize) {
            let _ = note(settings::set_custom_action(id));
        }
    }
    if let Ok(ahk) = GetDlgItem(Some(hwnd), ID_SHOT_ACTION_HK) {
        let sel = SendMessageW(ahk, CB_GETCURSEL, None, None).0;
        // Item 0's data is explicitly 0 ("(none)" — unbound), so reading item
        // data uniformly handles every index, curated or the appended unknown-
        // chord item alike.
        let packed = if sel < 0 {
            0
        } else {
            SendMessageW(ahk, CB_GETITEMDATA, Some(WPARAM(sel as usize)), None).0 as u32
        };
        let _ = note(settings::set_custom_action_hotkey(packed));
    }
}

/// Packed chord for the instant-screenshot combo: the enable checkbox is the on/off switch,
/// so off (or no selected item) yields 0 and the daemon skips a second hotkey.
unsafe fn quick_shot_chord(hwnd: HWND) -> u32 {
    if !checked(hwnd, ID_SHOT_QUICK_ENABLE) {
        return 0;
    }
    if let Ok(quick) = GetDlgItem(Some(hwnd), ID_SHOT_QUICK_HOTKEY) {
        let qsel = SendMessageW(quick, CB_GETCURSEL, None, None).0;
        if qsel >= 0 {
            return SendMessageW(quick, CB_GETITEMDATA, Some(WPARAM(qsel as usize)), None).0 as u32;
        }
    }
    0
}

/// Quick preview's master toggle + behavior prefs, then the screenshot enable checkbox -
/// `set_enabled` reconciles the daemon (start/stop + re-register) against everything
/// written above, so it must run last.
pub(super) unsafe fn apply_quick_preview_and_screenshot_enable(hwnd: HWND) {
    // Quick preview: persist the master toggle + behavior prefs. Written BEFORE
    // set_enabled() below so the daemon reconcile — which keeps the daemon resident
    // whenever Quick preview is enabled (via daemon_wanted) — sees the new state and
    // starts/stops the daemon + autostart entry to match.
    let _ = note(settings::set_preview_enabled(checked(
        hwnd,
        ID_PREVIEW_ENABLED,
    )));
    let _ = note(settings::set_preview_hold_peek(checked(
        hwnd,
        ID_PREVIEW_HOLD_PEEK,
    )));
    let _ = note(settings::set_preview_close_on_focus_loss(checked(
        hwnd,
        ID_PREVIEW_CLOSE_FOCUS,
    )));
    let _ = note(settings::set_preview_open_front(checked(
        hwnd,
        ID_PREVIEW_TOPMOST,
    )));
    let _ = note(settings::set_preview_blocked_exts(&blocked_exts_text(hwnd)));
    let _ = note(settings::set_preview_text(checked(hwnd, ID_PREVIEW_TEXT)));
    let _ = note(settings::set_preview_markdown(checked(
        hwnd,
        ID_PREVIEW_MARKDOWN,
    )));
    #[cfg(feature = "html-preview")]
    {
        let _ = note(settings::set_preview_html(checked(hwnd, ID_PREVIEW_HTML)));
        let _ = note(settings::set_preview_url_live(checked(
            hwnd,
            ID_PREVIEW_URL_LIVE,
        )));
    }

    let shot_on = checked(hwnd, ID_SHOT_ENABLE);
    // set_enabled persists the screenshot flag, then reconciles the daemon (start/stop +
    // re-register) accounting for the screenshot feature, the custom hotkey saved above,
    // AND Quick preview persisted just above — so it covers the "daemon needed only for a
    // custom hotkey / Quick preview" cases too.
    crate::screenshot::set_enabled(shot_on);
}

/// Per-format enable/disable flags: collect the changes against the (possibly filtered)
/// list model, persist them, then run the elevated re-register that rewrites the HKCR
/// shell hooks to match - rolling the flags back if that elevation is declined or fails,
/// so the persisted settings stay consistent with the (unchanged) hooks. Otherwise the two
/// silently diverge and, because change-detection reads HKCU, never reconcile.
pub(super) unsafe fn apply_format_flags(hwnd: HWND) {
    let mut changes: Vec<(&'static str, bool, bool)> = Vec::new();
    FMT_STATE.with(|st| {
        let st = st.borrow();
        for (i, &(ext, _)) in formats::FORMATS.iter().enumerate() {
            let want = st
                .get(i)
                .copied()
                .unwrap_or_else(|| settings::format_enabled(ext));
            let old = settings::format_enabled(ext);
            if old != want {
                changes.push((ext, want, old));
            }
        }
    });
    if changes.is_empty() {
        return;
    }
    for &(ext, want, _) in &changes {
        let _ = settings::set_format_enabled(ext, want);
    }
    // Only Ok counts - any other outcome means the HKCR hooks do NOT match the flags
    // we just wrote, so roll the flags back rather than leave the UI lying.
    if matches!(reregister_elevated(), Reg::Ok) {
        // The elevated re-register rewrote the MACHINE-WIDE hooks; it cannot write this
        // user's HKCU (it runs as the admin account). The per-user shell pieces are keyed
        // per format - the folder verb, and the corner-icon overlay that
        // `typeoverlay::sync` derives from the format list - so a format switched on or off
        // here leaves them stale until something else happens to run. We ARE the original
        // user, so do it in-process, the same call the installer makes.
        let _ = sagethumbs2k_core::register::sync_user_shell();
    } else {
        for &(ext, _, old) in &changes {
            let _ = settings::set_format_enabled(ext, old);
        }
        // The rollback above reverted HKCU, but FMT_STATE (the model the checklist
        // paints from) and the ID_LIST checkboxes still show the attempted state the
        // registry write just reverted — refresh both so the list matches reality.
        revert_format_list_view(hwnd);
        message_box(hwnd, t("msg_admin_required"), "SageThumbs 2K");
    }
}

/// Re-seed [`FMT_STATE`] from the persisted per-format flags and repopulate the file-type
/// list from it, preserving whatever the filter box currently shows. Used after a rollback
/// (Save or Import) so the checklist reflects the registry state that actually won, not the
/// attempted state that was just reverted.
pub(super) unsafe fn revert_format_list_view(hwnd: HWND) {
    seed_format_state();
    if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
        let text = get_edit_text(hwnd, ID_SEARCH);
        populate_list(list, &text);
    }
}

/// `SHChangeNotify(SHCNE_ASSOCCHANGED)` — tells Explorer file-type handlers changed,
/// so it re-reads context-menu registrations rather than serving a stale cache. The
/// standard post-settings nudge for a shell extension; cheap and side-effect-free.
pub(super) fn notify_shell_assoc_changed() {
    use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}
