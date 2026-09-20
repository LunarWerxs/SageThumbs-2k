//! What each page holds: the row table per category and the ids a row's controls carry.

use super::*;

/// One row in a category's content pane. Ids reference controls already created by
/// `build_controls`; the layout just repositions them.
#[derive(Clone, Copy)]
pub(in super::super) enum Row {
    Head(i32),                // group sub-header (owner-draw static)
    Switch(i32),              // a checkbox, drawn as a toggle switch
    Pair(i32, i32, i32, i32), // label, field, field_w, field_h (combo field_h>40)
    Btn(i32, i32),            // button id, width
    BtnStatus(i32, i32, i32), // button (left) + right-aligned status, one row: btn_id, btn_w, status_id
    StatusBtn(i32, i32, i32), // status (left, fills) + right-aligned button, one row: status_id, btn_id, btn_w
    Status(i32),              // dynamic status line
    Btn3(i32, i32, i32),      // three equal buttons on one row
    Wide(i32),                // a full-width control (search edit)
    WideBtn(i32, i32, i32), // a wide edit (left, fills) + right-aligned button, one row: edit_id, btn_id, btn_w
    ListFill(i32),          // a list that fills down to the footer
}

// Category order: General (Thumbnails+General merged) · File types · Ebook/comic ·
// Right-click menu · Screenshots · Advanced.
/// General = the merged Thumbnails + General (Custom action is its own tab now).
pub(super) const GENERAL: [Row; 11] = {
    use Row::*;
    [
        // Portable copies only (installed builds take GENERAL[1..]): the per-user
        // Explorer registration, HOME AT LAST. Its Advanced-page comment always said it
        // belonged here by topic and only lived there for space — the badge switches
        // moving to the Appearance page is what finally made the room. First row on
        // purpose: on a portable copy this is the switch everything else depends on.
        BtnStatus(ID_PORTABLE_REG, 240, ID_PORTABLE_REG_STATUS),
        // Every page opens with a header now (uniformity was the ask): this one re-uses
        // the old left-column "Thumbnails" label the v3 layout used to hide.
        Head(ID_LBL_THUMBS),
        Switch(ID_ENABLE_THUMBS),
        Switch(ID_USE_EMBEDDED),
        Head(ID_LBL_LIMITS),
        Pair(ID_LBL_MAXFILE, ID_MAXSIZE, 84, 18),
        Pair(ID_LBL_MAXTHUMB, ID_SIZE, 84, 18),
        Pair(ID_LBL_JPEG, ID_JPEG, 84, 18),
        Pair(ID_LBL_PNG, ID_PNG, 84, 18),
        Head(ID_LBL_GENERAL), // "Language & files"
        Pair(ID_LBL_LANG, ID_LANG, 156, 200),
    ]
};

/// Advanced — system behaviors only: Diagnostics / Updates / Hotkey service.
/// (Settings sync + Backup moved to their own "Data & Backup" tab.) Unlike GENERAL, this
/// list is NOT sliced for installed builds — `cat_rows`' `7 => &ADVANCED` arm is
/// unconditional, and ADVANCED[0] is the Diagnostics header, not a portable-only row.
pub(super) const ADVANCED: [Row; 11] = {
    use Row::*;
    [
        Head(ID_LBL_DIAG),
        Switch(ID_VERBOSE_LOG),
        Btn(ID_OPEN_LOG, 320),
        Btn(ID_REBUILD_CACHE, 320),
        Btn(ID_REPAIR_ASSOC, 320),
        Btn(ID_RUN_DOCTOR, 320),
        Head(ID_LBL_UPDATES),
        Switch(ID_UPDATE_AUTO),
        Btn(ID_CHECK_UPDATES, 184),
        // The service's state + Restart moved to the Screenshots page (see category 4);
        // what stays here is the one genuinely system-level preference it owns.
        Head(ID_LBL_HOTKEY_SVC),
        Switch(ID_SHOT_HIDE_TRAY),
    ]
};

/// Every field id from every page's `Row::Pair` rows, split into the two rounded-frame
/// shapes `restyle::paint_chrome` draws them with: numeric edits (`field_h <= 40`, the
/// digit-biased 4/6/2 frame) and combos (`field_h > 40`, the symmetric 4/2/2 frame) — see
/// the `Row::Pair` doc comment for the `field_h > 40` convention. `paint_chrome` used to
/// hand-list these ids itself, so a new `Pair` row (`ID_VIDEO_OFFSET`, `ID_SHOT_QUICK_HOTKEY`,
/// `ID_SHOT_DELAY`, `ID_SHOT_ACTION`, `ID_SHOT_ACTION_HK`, `ID_CORNER_MARK`) got created and
/// laid out but never framed, because nothing forced the two lists to stay in sync with the
/// row data they were duplicating. Deriving them here means a page can never again add a
/// Pair row this misses.
/// Bucket a single row's `Pair` field id into `edits`/`combos` by the `field_h > 40`
/// convention (numeric edits vs combos); non-`Pair` rows are ignored. Keeps the two
/// buckets free of duplicates.
pub(super) fn collect_pair_field(row: Row, edits: &mut Vec<i32>, combos: &mut Vec<i32>) {
    if let Row::Pair(_, field, _, field_h) = row {
        let bucket = if field_h > 40 { combos } else { edits };
        if !bucket.contains(&field) {
            bucket.push(field);
        }
    }
}

pub(in super::super) fn pair_field_ids() -> (Vec<i32>, Vec<i32>) {
    let mut edits = Vec::new();
    let mut combos = Vec::new();
    for ci in 0..NCAT {
        for &row in cat_rows(ci) {
            collect_pair_field(row, &mut edits, &mut combos);
        }
    }
    (edits, combos)
}

/// Every full-width TEXT edit from every page's `Row::Wide` / `Row::WideBtn` rows: the ones
/// `restyle::paint_chrome` frames with the text-centred 5/3 pads. Derived for the reason
/// [`pair_field_ids`] is: `paint_chrome` named `ID_SEARCH` by hand, so the licence-key edit
/// (`Row::WideBtn`, laid out around "its 5/3 frame" - see `place_wide_btn_row`) shipped with no
/// frame at all, a bare white strip beside a rounded button.
pub(in super::super) fn wide_edit_ids() -> Vec<i32> {
    let mut edits = Vec::new();
    for ci in 0..NCAT {
        for &row in cat_rows(ci) {
            if let Row::Wide(id) | Row::WideBtn(id, _, _) = row {
                if !edits.contains(&id) {
                    edits.push(id);
                }
            }
        }
    }
    edits
}

pub(in super::super) fn cat_rows(ci: usize) -> &'static [Row] {
    use Row::*;
    match ci {
        0 if sagethumbs2k_core::settings::portable() => &GENERAL,
        0 => &GENERAL[1..],
        1 => &[
            // Appearance: every "what does the tile look like" switch in ONE place.
            // These used to be scattered — the badge trio on General (FIXED and full),
            // the overlay + video-cover rows exiled to File types. One page ends the
            // scavenger hunt and frees both donors.
            Head(ID_LBL_TILE_LOOK),
            // The corner question first, because it is the one real decision on this page and
            // the style switch under it is meaningless until it is answered.
            // Wider than the 156 the other combos use: its options are noun phrases naming a
            // thing ("Windows' file-type icon"), not one-word modes like "Light"/"Dark", and at
            // 156 the closed box clipped the SageThumbs option mid-word. There is ~220px of
            // clear gap between this label and the field, so the extra 60 costs nothing.
            Pair(ID_LBL_CORNER_MARK, ID_CORNER_MARK, 216, 200),
            // How big our mark is drawn - directly under the question it answers, and greyed
            // with the style switch below it whenever the corner is not our mark.
            Pair(ID_LBL_BADGE_SIZE, ID_BADGE_SIZE, 156, 200),
            Switch(ID_BADGE_ICON),
            Switch(ID_THUMB_CHECKER),
            Switch(ID_VIDEO_COVER_ART),
            // Reads as the follow-up to the switch above: "or, if you'd rather have a frame,
            // here is which one." General could not take it (that page is already within a
            // row of its footer), and this is where it belongs by topic anyway.
            Pair(ID_LBL_VIDEO_OFFSET, ID_VIDEO_OFFSET, 84, 18),
        ],
        2 => &[
            // File types: purely "which formats", with the appearance strays gone.
            Head(ID_LBL_FORMATS_PICK),
            Btn3(ID_SELECT_ALL, ID_CLEAR_ALL, ID_DEFAULTS),
            Wide(ID_SEARCH),
            ListFill(ID_LIST),
        ],
        3 => &[
            // Ebook/comic (plus the generic-archive contact sheet).
            Head(ID_LBL_EBOOK),
            Switch(ID_C_SORT),
            Switch(ID_C_PREFER_COVER),
            Switch(ID_C_SKIP_SCAN),
            Switch(ID_C_ARCHIVE_SHEET),
            // PDF page margin moved here from General (2026-08-05, when the format badge
            // needed General's last row). It is a DOCUMENT rendering option and had been
            // filed under "Language & files", so this reads better than where it was.
            Switch(ID_PDF_MARGIN),
        ],
        4 => &[
            Head(ID_LBL_MENU_LOOK),
            Switch(ID_ENABLE_MENU),
            Switch(ID_MENU_ALL_TYPES),
            Switch(ID_MENU_QUICK),
            // This controls the transparency backdrop of the context-menu preview,
            // so keep it with that surface instead of crowding the General page.
            Switch(ID_MENU_CHECKER),
            // Folder right-click verb ("Build thumbnails here") — a menu-presence switch
            // like the ones above it, not a verb-behavior one, so it stays in this group.
            Switch(ID_FOLDER_PREBUILD),
            // "Converting & resizing": these two govern what the Convert/Resize VERBS do
            // to a file, not what the menu looks like — the header makes that seam
            // visible instead of leaving four menu switches running into two file rows.
            Head(ID_LBL_CONVERT_VERBS),
            Switch(ID_PRESERVE_DATE),
            Switch(ID_KEEP_METADATA),
            Pair(ID_LBL_PREVIEW, ID_MENU_PREVIEW, 156, 200),
            // The menu-items checklist lives in its own popup editor now (see
            // `menuitems.rs`): on-page it was what kept this page cramped, and a
            // drag-to-reorder list competes badly with a page that also has sections.
            Head(ID_LBL_MENU_ITEMS),
            Btn(ID_MENU_ITEMS_EDIT, 200),
        ],
        5 => &[
            // Screenshots — custom action lives on its own tab; "Hide tray icon" on Advanced.
            Head(ID_LBL_SHOT),
            Switch(ID_SHOT_ENABLE),
            Switch(ID_SHOT_QUICK_ENABLE),
            Switch(ID_SHOT_USE_DIR),
            Pair(ID_LBL_SHOT_HK, ID_SHOT_HOTKEY, 156, 200),
            Pair(ID_LBL_SHOT_QUICK_HK, ID_SHOT_QUICK_HOTKEY, 156, 200),
            Pair(ID_LBL_SHOT_TOOL, ID_SHOT_TOOL, 156, 200),
            Pair(ID_LBL_SHOT_DELAY, ID_SHOT_DELAY, 156, 200),
            // Service state + Restart, moved back here from Advanced 2026-08-06. A hotkey only
            // fires while the helper is resident, so "is it running / put it back" is part of
            // this feature, not a system setting. Being on Advanced made it unfindable by the
            // one person who needs it: issue #14's reporter had the entry deleted by antivirus,
            // and the page reporting the hotkey as ON offered no way to see or fix that. It is
            // also what makes the row directly above it honest, since that switch can read ON
            // while nothing is actually listening.
            BtnStatus(ID_SHOT_RESTART, 184, ID_SHOT_STATUS),
            Status(ID_SHOT_DIR),
            Btn(ID_SHOT_SET_DIR, 150),
            Btn(ID_EDIT_UPLOAD_HOSTS, 184),
        ],
        6 => &[
            // Quick action — bind a global hotkey to run a tool.
            Head(ID_LBL_QUICKACTION),
            Switch(ID_CUSTOM_ACTION_ENABLE),
            Pair(ID_LBL_SHOT_ACTION, ID_SHOT_ACTION, 156, 200),
            Pair(ID_LBL_SHOT_ACTION_HK, ID_SHOT_ACTION_HK, 156, 200),
        ],
        7 => &ADVANCED,
        // Quick preview — QuickLook-style "press Space, see the file". The master toggle drives
        // daemon residency (like Screenshots); the rest are viewer prefs. The HTML/.url rows only
        // exist when the `html-preview` feature is compiled in: ONE list, with the two rows
        // gated in place, because this page used to exist twice (one copy per build) and a
        // control added to only one of them was invisible in whichever build you were not
        // looking at - exactly how the theme row came to be missing the first time.
        8 => &[
            Head(ID_LBL_PREVIEW_BEHAVIOR),
            Switch(ID_PREVIEW_ENABLED),
            Switch(ID_PREVIEW_HOLD_PEEK),
            Switch(ID_PREVIEW_CLOSE_FOCUS),
            Switch(ID_PREVIEW_TOPMOST),
            // Light/dark for SageThumbs' own windows. Sits with preview BEHAVIOUR rather than
            // under "Also preview" below, which is a list of content-type opt-ins.
            Pair(ID_LBL_APP_THEME, ID_APP_THEME, 156, 200),
            // Per-extension blocklist — behavior (what Quick preview refuses to try),
            // not a content-type opt-in, so it sits above the "Also preview" split.
            Pair(
                ID_LBL_PREVIEW_BLOCKED_EXTS,
                ID_PREVIEW_BLOCKED_EXTS,
                156,
                18,
            ),
            // "Also preview": behavior above, content-type opt-ins below — the split
            // matches how the decision is actually made ("turn it on" vs "and also my
            // markdown files"), instead of eight equal-looking rows.
            Head(ID_LBL_PREVIEW_KINDS),
            Switch(ID_PREVIEW_TEXT),
            Switch(ID_PREVIEW_MARKDOWN),
            #[cfg(feature = "html-preview")]
            Switch(ID_PREVIEW_HTML),
            #[cfg(feature = "html-preview")]
            Switch(ID_PREVIEW_URL_LIVE),
        ],
        9 => &[
            // Data & Backup — settings portability: optional cloud sync + local backup/restore.
            // Controls are created in build_controls; listing them here places them into this
            // pane + registers them for nav show/hide.
            Head(ID_LBL_SYNC),
            StatusBtn(ID_SYNC_STATUS, ID_SYNC_BTN, 160),
            Head(ID_LBL_BACKUP),
            Btn(ID_RESET_ALL, 320),
            Btn(ID_IMPORT, 320),
            Btn(ID_EXPORT, 320),
        ],
        _ => &[
            // Licence — the business-seat key: what this copy currently believes about
            // itself (mode + licence state), the key and its Redeem button on one row, the
            // three doors (check, move, buy) on one row, and a last row shared by the two
            // lines that are never wanted at once. See `settings_dlg/licence_ui.rs`, which
            // decides the tones and what shows. Re-shaped 2026-09-15 from a stack of four
            // left-aligned buttons under a bare edit (Michael: "boring, bland").
            Head(ID_LBL_LICENCE),
            Status(ID_LICENCE_MODE_STATUS),
            Status(ID_LICENCE_STATE_STATUS),
            Status(ID_LICENCE_UPDATES_STATUS),
            Head(ID_LBL_LICENCE_KEY),
            WideBtn(ID_LICENCE_KEY_EDIT, ID_LICENCE_REDEEM_BTN, 160),
            Status(ID_LICENCE_REDEEM_STATUS),
            // Check, Move (the self-serve rebind door, always visible - see ID_LICENCE_MOVE's
            // doc), Buy: Buy draws as the accent button while this copy has no licence.
            Btn3(ID_LICENCE_CHECK_NOW, ID_LICENCE_MOVE, ID_LICENCE_BUY),
            // The last two rows are mutually exclusive: the "using it at work?" line shows on a
            // Personal copy without a key, the Renew button on a licensed machine near or past
            // its updates window; never both. The hint needs the full pane width (it ran to
            // "US$49 per" when it shared a row with the button), so it is its own row, and the
            // one hole this can leave - 22px above Renew on a licensed machine, where the hint
            // is hidden - is breathing room, not a gap in the middle of the page.
            Status(ID_LICENCE_WORK_HINT),
            Btn(ID_LICENCE_RENEW, 184),
        ],
    }
}

/// Advance the design-pixel cursor for rows whose height is independent of the
/// live window/control geometry. List rows return `None` because their height is
/// calculated against the footer or measured HWND at runtime.
pub(super) fn fixed_row_next_y(row: Row, y: i32, first: bool) -> Option<i32> {
    match row {
        Row::Head(_) => Some(y + if first { 26 } else { 46 }),
        Row::Switch(_) => Some(y + 32),
        Row::Pair(..) => Some(y + 34),
        Row::Btn(..) | Row::BtnStatus(..) | Row::StatusBtn(..) => Some(y + 32),
        Row::Status(_) => Some(y + 22),
        Row::Btn3(..) => Some(y + 34),
        // 8 above the edit + the 5/3 frame around an 18px box = 29, then the same ~11px
        // gap under it the 24px box used to leave. (Was 44, for the taller box.)
        Row::Wide(_) => Some(y + 40),
        // Same rhythm as `Wide`: the edit sits where a wide edit sits, the button beside it
        // is centred on the same line, and the row costs what a wide row costs.
        Row::WideBtn(..) => Some(y + 40),
        Row::ListFill(_) => None,
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    /// `CAT_LICENCE` is what `licence_ui` gates its page-only rows on, so it must name the page
    /// `cat_rows` actually lays those rows out on - the `_` arm - and stay there when a page is
    /// inserted above it.
    #[test]
    fn licence_is_the_last_category() {
        assert_eq!(nav_key(CAT_LICENCE), "nav_licence");
        assert_eq!(category_index("nav_licence"), Some(CAT_LICENCE));
        assert_eq!(CAT_LICENCE, NCAT - 1);
    }

    #[test]
    fn fixed_pages_keep_space_above_the_footer() {
        // The fixed 588px design shell leaves footer_y at least 496px at 96 DPI
        // after its non-client frame. Test that conservative floor; production
        // still checks the actual live client rectangle as an additional guard.
        const MIN_FOOTER_Y: i32 = 496;

        fn bottom_of(rows: &[Row], label: &str) -> i32 {
            let mut y = PANE_TOP + PANE_HEAD_H + 8;
            let mut first = true;
            for &row in rows {
                let Some(next_y) = fixed_row_next_y(row, y, first) else {
                    panic!("fixed settings page {label} unexpectedly contains a list row");
                };
                y = next_y;
                first = false;
            }
            y
        }

        for ci in 0..NCAT {
            if ci == 2 {
                continue; // File Types fills its list to the footer by design.
            }
            let label = ci.to_string();
            let y = bottom_of(cat_rows(ci), &label);
            assert!(
                y <= MIN_FOOTER_Y - 12,
                "settings category {ci} reaches the footer ({y} > {})",
                MIN_FOOTER_Y - 12
            );
        }

        // `cat_rows(0)` returns whichever General variant matches THIS process, and a test
        // run is never portable — so the taller portable page (the extra registration row)
        // would otherwise never be measured, and would overflow only on a user's machine.
        let y = bottom_of(&GENERAL, "General (portable)");
        assert!(
            y <= MIN_FOOTER_Y - 12,
            "the portable General page reaches the footer ({y} > {})",
            MIN_FOOTER_Y - 12
        );
    }
}
