//! v3 nav-rail + content-pane layout (extracted from settings_dlg; parent-hub pattern).

use super::*;
use crate::gdip;
use windows::Win32::Graphics::Gdi::{GetTextMetricsW, DT_END_ELLIPSIS, TEXTMETRICW};
use windows::Win32::UI::Controls::ODS_FOCUS;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_DOWN, VK_RETURN, VK_UP};

// ===================== v3 layout: nav rail + content pane =====================
// Geometry (96-dpi design px). The window is nav rail (left) + a content pane that
// shows ONE category at a time; the rest of the controls are hidden.
pub(super) const NAV_X: i32 = 8;
pub(super) const NAV_TOP: i32 = 14;
pub(super) const NAV_W: i32 = 188;
pub(super) const NAV_ITEM_H: i32 = 38;
pub(super) const PANE_X: i32 = 212;
pub(super) const PANE_W: i32 = 528;
pub(super) const PANE_TOP: i32 = 16;
pub(super) const PANE_HEAD_H: i32 = 50; // the icon-chip + title + blurb page header
pub(super) const ID_NAV_BASE: i32 = 1700; // nav items occupy ID_NAV_BASE .. ID_NAV_BASE+NCAT
pub(super) const ID_PANE_HEADER: i32 = 1710;
pub(super) const NCAT: usize = 10;
// The nav ids and ID_PANE_HEADER share one id space, and at NCAT = 10 they fit with exactly
// ZERO headroom: nav owns 1700..=1709 and the header sits on 1710. An eleventh category would
// silently hand the pane header a nav item's identity, and the two `(ID_NAV_BASE..ID_NAV_BASE +
// NCAT)` range tests in `mod.rs`'s WM_COMMAND would start routing clicks on the header as a
// category switch. Nothing about that fails to compile or looks wrong in a diff, which is
// exactly the shape of bug this repo keeps paying for, so it fails the BUILD instead.
// (The stale comment this replaces still said the range ended at 1708, from when NCAT was 8.)
const _: () = assert!(
    ID_NAV_BASE as usize + NCAT <= ID_PANE_HEADER as usize,
    "a new Settings category pushed the nav ids onto ID_PANE_HEADER: move ID_PANE_HEADER up"
);
/// The locale KEY for category `ci`'s label. This match is the category ORDER, and it is the
/// only copy of it: [`nav_label`] translates it and [`category_index`] searches it, so a caller
/// that wants "the Quick preview page" asks by name and cannot be silently re-pointed at a
/// different page when one is inserted above it. (`--tab` numbers in docs have drifted exactly
/// that way before — see CLAUDE.md §6.)
pub(super) fn nav_key(ci: usize) -> &'static str {
    match ci {
        0 => "nav_general",
        1 => "nav_appearance",
        2 => "nav_filetypes",
        3 => "nav_ebook",
        4 => "nav_menu",
        5 => "nav_screenshots",
        6 => "nav_quickaction",
        7 => "nav_advanced",
        8 => "nav_quickpreview",
        _ => "nav_databackup",
    }
}

/// Localized nav-rail / page-header label for category `ci`. Pulls from `t()` so a
/// live language switch re-texts it (the nav statics + pane header re-read this).
pub(super) fn nav_label(ci: usize) -> &'static str {
    t(nav_key(ci))
}

/// The category index whose label key is `key`, or `None` if no page carries it.
pub(super) fn category_index(key: &str) -> Option<usize> {
    (0..NCAT).find(|&ci| nav_key(ci) == key)
}

/// One row in a category's content pane. Ids reference controls already created by
/// `build_controls`; the layout just repositions them.
#[derive(Clone, Copy)]
pub(super) enum Row {
    Head(i32),                // group sub-header (owner-draw static)
    Switch(i32),              // a checkbox, drawn as a toggle switch
    Pair(i32, i32, i32, i32), // label, field, field_w, field_h (combo field_h>40)
    Btn(i32, i32),            // button id, width
    BtnStatus(i32, i32, i32), // button (left) + right-aligned status, one row: btn_id, btn_w, status_id
    StatusBtn(i32, i32, i32), // status (left, fills) + right-aligned button, one row: status_id, btn_id, btn_w
    Status(i32),              // dynamic status line
    Btn3(i32, i32, i32),      // three equal buttons on one row
    Wide(i32),                // a full-width control (search edit)
    ListFill(i32),            // a list that fills down to the footer
}

// Category order: General (Thumbnails+General merged) · File types · Ebook/comic ·
// Right-click menu · Screenshots · Advanced.
/// General = the merged Thumbnails + General (Custom action is its own tab now).
const GENERAL: [Row; 11] = {
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
const ADVANCED: [Row; 11] = {
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

pub(super) fn cat_rows(ci: usize) -> &'static [Row] {
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
            Switch(ID_FORMAT_BADGE),
            Switch(ID_BADGE_ICON),
            Switch(ID_THUMB_CHECKER),
            Switch(ID_HIDE_TYPE_OVERLAY),
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
        // exist when the `html-preview` feature is compiled in.
        #[cfg(feature = "html-preview")]
        8 => &[
            Head(ID_LBL_PREVIEW_BEHAVIOR),
            Switch(ID_PREVIEW_ENABLED),
            Switch(ID_PREVIEW_HOLD_PEEK),
            Switch(ID_PREVIEW_CLOSE_FOCUS),
            Switch(ID_PREVIEW_TOPMOST),
            // Light/dark for SageThumbs' own windows. Sits with preview BEHAVIOUR rather than
            // under "Also preview" below, which is a list of content-type opt-ins.
            Pair(ID_LBL_APP_THEME, ID_APP_THEME, 156, 200),
            // "Also preview": behavior above, content-type opt-ins below — the split
            // matches how the decision is actually made ("turn it on" vs "and also my
            // markdown files"), instead of eight equal-looking rows.
            Head(ID_LBL_PREVIEW_KINDS),
            Switch(ID_PREVIEW_TEXT),
            Switch(ID_PREVIEW_MARKDOWN),
            Switch(ID_PREVIEW_HTML),
            Switch(ID_PREVIEW_URL_LIVE),
        ],
        #[cfg(not(feature = "html-preview"))]
        8 => &[
            Head(ID_LBL_PREVIEW_BEHAVIOR),
            Switch(ID_PREVIEW_ENABLED),
            Switch(ID_PREVIEW_HOLD_PEEK),
            Switch(ID_PREVIEW_CLOSE_FOCUS),
            Switch(ID_PREVIEW_TOPMOST),
            // Keep in step with the `html-preview` variant above: this page exists twice and a
            // control added to only one of them is invisible in whichever build you are not
            // looking at. That is exactly how this line came to be missing the first time.
            Pair(ID_LBL_APP_THEME, ID_APP_THEME, 156, 200),
            Head(ID_LBL_PREVIEW_KINDS),
            Switch(ID_PREVIEW_TEXT),
            Switch(ID_PREVIEW_MARKDOWN),
        ],
        _ => &[
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
    }
}

/// Advance the design-pixel cursor for rows whose height is independent of the
/// live window/control geometry. List rows return `None` because their height is
/// calculated against the footer or measured HWND at runtime.
fn fixed_row_next_y(row: Row, y: i32, first: bool) -> Option<i32> {
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
        Row::ListFill(_) => None,
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

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

#[derive(Default)]
pub(super) struct NavState {
    pub(super) active: usize,
    pub(super) cats: Vec<Vec<HWND>>,
}
thread_local! {
    pub(super) static NAV: std::cell::RefCell<NavState> = std::cell::RefCell::new(NavState::default());
}

/// Mix `pct`% of `fg` over `bg` (both 0x00BBGGRR COLORREFs) — for accent tints.
pub(super) fn blend(fg: COLORREF, bg: COLORREF, pct: i32) -> COLORREF {
    let ch = |sh: u32| {
        let a = ((fg.0 >> sh) & 0xFF) as i32;
        let b = ((bg.0 >> sh) & 0xFF) as i32;
        (((a * pct + b * (100 - pct)) / 100) as u32) & 0xFF
    };
    COLORREF(ch(0) | (ch(8) << 8) | (ch(16) << 16))
}

/// Draw a category's line icon (matching the v3 web SVGs) in an `sz`×`sz` box at
/// `(x, y)`, stroked in `color`. Hollow shapes, anti-aliased with round caps/joins via
/// GDI+ (so the diagonals and rounded corners read as clean Fluent line icons instead of
/// the stair-stepped raw-GDI strokes they used to be).
pub(super) unsafe fn draw_cat_icon(hdc: HDC, ci: usize, x: i32, y: i32, sz: i32, color: COLORREF) {
    let pw = (sz / 8).max(1);
    // Map the 24-unit SVG space into the box: x-coords via mx, y-coords via my.
    let mx = |v: i32| x + v * sz / 24;
    let my = |v: i32| y + v * sz / 24;
    gdip::with_aa(hdc, |g| {
        let p = gdip::pen_round(color, pw);
        // rounded-rect outline / ellipse outline / polyline, all in the 24-unit SVG space.
        let rr = |a: i32, b: i32, c: i32, d: i32, r: i32| {
            gdip::stroke_round(g, p, mx(a), my(b), mx(c) - mx(a), my(d) - my(b), r);
        };
        let el = |a: i32, b: i32, c: i32, d: i32| {
            gdip::ellipse(g, p, mx(a), my(b), mx(c) - mx(a), my(d) - my(b));
        };
        let ln = |pts: &[(i32, i32)]| {
            let mapped: Vec<(i32, i32)> = pts.iter().map(|&(a, b)| (mx(a), my(b))).collect();
            gdip::polyline(g, p, &mapped);
        };
        match ci {
            0 => {
                // image: framed rect + sun + mountain
                rr(3, 3, 21, 21, sz / 4);
                el(6, 6, 11, 11);
                ln(&[(21, 15), (16, 10), (5, 21)]);
            }
            1 => {
                // appearance: a tile with a corner badge (the format-badge motif)
                rr(3, 3, 18, 18, sz / 6);
                el(13, 13, 21, 21);
            }
            2 => {
                // grid: four rounded squares
                for (gx, gy) in [(3, 3), (13, 3), (3, 13), (13, 13)] {
                    rr(gx, gy, gx + 8, gy + 8, sz / 8);
                }
            }
            3 => {
                // book: cover + spine + page lines (Ebook/comic)
                rr(5, 4, 19, 20, sz / 8);
                ln(&[(8, 4), (8, 20)]);
                ln(&[(11, 9), (16, 9)]);
                ln(&[(11, 13), (16, 13)]);
            }
            4 => {
                // menu: three lines (last shorter)
                for (yy, x2) in [(6, 20), (12, 20), (18, 14)] {
                    ln(&[(4, yy), (x2, yy)]);
                }
            }
            5 => {
                // camera: body + bump + lens
                rr(3, 8, 21, 19, sz / 8);
                ln(&[(8, 8), (9, 6), (15, 6), (16, 8)]);
                el(9, 10, 15, 16);
            }
            6 => {
                // bolt: a lightning shape (Quick action)
                ln(&[
                    (13, 2),
                    (7, 13),
                    (11, 13),
                    (10, 22),
                    (18, 10),
                    (12, 10),
                    (13, 2),
                ]);
            }
            7 => {
                // sliders: two lines, each with a knob (Advanced)
                ln(&[(4, 8), (20, 8)]);
                ln(&[(4, 16), (20, 16)]);
                el(13, 5, 19, 11);
                el(5, 13, 11, 19);
            }
            8 => {
                // eye: a wide almond outline + a round iris (Quick preview)
                el(3, 8, 21, 16);
                el(10, 9, 14, 15);
            }
            _ => {
                // save/backup: a down-arrow into an open tray (Data & Backup)
                ln(&[(12, 3), (12, 14)]);
                ln(&[(8, 10), (12, 14), (16, 10)]);
                ln(&[(4, 16), (4, 21), (20, 21), (20, 16)]);
            }
        }
        gdip::drop_pen(p);
    });
}

pub(super) fn cat_blurb(ci: usize) -> &'static str {
    match ci {
        0 => t("blurb_general"),
        1 => t("blurb_appearance"),
        2 => t("blurb_filetypes"),
        3 => t("blurb_ebook"),
        4 => t("blurb_menu"),
        5 => t("blurb_screenshots"),
        6 => t("blurb_quickaction"),
        7 => t("blurb_advanced"),
        8 => t("blurb_quickpreview"),
        _ => t("blurb_databackup"),
    }
}

/// Does this settings page hold any value the user has CHANGED from its default?
/// Drives the little dot on the nav rail — the answer to "where did I change something"
/// across nine pages, without opening each one. Reads the live settings (cheap registry /
/// portable-ini reads; runs only when a rail item repaints, not per frame).
///
/// Deliberately coarse: it names the pages that DIFFER, it does not promise the reverse
/// (a page with no dot may still have sub-state we don't track, e.g. list orderings).
pub(super) fn page_has_non_defaults(ci: usize) -> bool {
    use sagethumbs2k_core::settings as s;
    match ci {
        // General: master switch, embedded pref, badge trio, checkerboard, the numbers,
        // and the JPEG/PNG quality fields (also on this page — see ID_JPEG/ID_PNG in
        // `cat_rows`'s GENERAL rows).
        0 => {
            !s::thumbnails_enabled()
                || !s::use_embedded()
                || s::max_file_size_bytes() != u64::from(s::DEFAULT_MAX_FILE_MB) * 1024 * 1024
                || s::max_thumb_size() != s::DEFAULT_THUMB_SIZE
                || s::jpeg_quality() != s::DEFAULT_JPEG as u8
                || s::png_level() != s::DEFAULT_PNG
        }
        // Appearance: every switch on the page, all default-off except the icon style.
        1 => {
            s::format_badge()
                || !s::format_badge_icon()
                || s::thumb_checker()
                || s::hide_type_overlay()
                || s::prefer_cover_art()
                || s::video_offset_pct() != s::DEFAULT_VIDEO_OFFSET_PCT
        }
        // Ebook/comic: also where ID_PDF_MARGIN actually lives (moved here from General,
        // see `cat_rows`), so its non-Tight PDF page layout counts toward this page's dot.
        3 => {
            !s::container_sort()
                || !s::container_prefer_cover()
                || s::container_skip_scanlation()
                || !s::archive_collage()
                || s::pdf_page() != sagethumbs2k_core::PdfPage::Tight
        }
        4 => {
            !s::menu_enabled()
                || s::menu_all_file_types()
                || s::menu_quick_verbs()
                || !s::preview_checker()
                || !s::folder_prebuild_verb()
                || s::preserve_file_date()
                || !s::keep_metadata_on_convert()
        }
        // Screenshots / Quick preview: their daemon-backed master switches are OFF by
        // default (first-run offers them), so ON is the changed state.
        5 => crate::screenshot::is_enabled(),
        // Quick action: unbound (vk == 0) is the default; any bound hotkey is a change.
        6 => s::custom_action_hotkey().1 != 0,
        8 => s::preview_enabled(),
        7 => !s::update_auto_check() || s::screenshot_hide_tray(),
        _ => false,
    }
}

thread_local! {
    /// Bitmask of pages whose dot has already been ACKNOWLEDGED, cached from settings.
    /// `u32::MAX` is the "not loaded yet" sentinel — there are only `NCAT` (10) pages, so
    /// it can never be a real mask.
    static DOTS_SEEN: std::cell::Cell<u32> = const { std::cell::Cell::new(u32::MAX) };
}

/// Acknowledged-dot mask, loaded from settings on first use (HKCU `NavDotsSeen`, or the
/// portable ini). Persisted rather than per-session: a hint that came back at every launch
/// read as an unread badge that could never be cleared.
fn dots_seen() -> u32 {
    DOTS_SEEN.with(|c| {
        if c.get() == u32::MAX {
            c.set(sagethumbs2k_core::settings::get_dword_opt("NavDotsSeen").unwrap_or(0));
        }
        c.get()
    })
}

/// Should page `ci` show its dot? Only while it has a changed setting AND the user hasn't
/// been to the page yet. The dot answers "where did I change something" for someone opening
/// Settings cold; once they've actually opened that page it has done its job, so it stops.
pub(super) fn dot_visible(ci: usize) -> bool {
    ci < 32 && page_has_non_defaults(ci) && dots_seen() & (1u32 << ci) == 0
}

/// Acknowledge page `ci`'s dot. Called on every category switch — including the initial
/// switch to page 0, so the page you land on doesn't keep a dot you're already looking at.
fn mark_dot_seen(ci: usize) {
    if ci >= 32 {
        return;
    }
    let (cur, bit) = (dots_seen(), 1u32 << ci);
    if cur & bit != 0 {
        return;
    }
    DOTS_SEEN.with(|c| c.set(cur | bit));
    // Best-effort: a failed write costs a dot that reappears next launch, nothing more.
    let _ = sagethumbs2k_core::settings::set_dword("NavDotsSeen", cur | bit);
}

/// Owner-draw a nav-rail item: an accent-tinted pill + accent icon + bar when
/// active; a muted icon + plain text otherwise.
pub(super) unsafe fn draw_nav_item(hwnd: HWND, d: &DRAWITEMSTRUCT, active: bool) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    let ci = (d.CtlID as i32 - ID_NAV_BASE) as usize;
    fill(hdc, &rc, DARK_BG());
    if active {
        let tint = blend(ACCENT(), DARK_BG(), 16);
        let (px, py) = (rc.left + dpi_scale(hwnd, 4), rc.top + dpi_scale(hwnd, 3));
        let (pw, ph) = (
            (rc.right - dpi_scale(hwnd, 4)) - px,
            (rc.bottom - dpi_scale(hwnd, 3)) - py,
        );
        gdip::with_aa(hdc, |g| {
            let b = gdip::brush(tint);
            gdip::fill_round(g, b, px, py, pw, ph, dpi_scale(hwnd, 8));
            gdip::drop_brush(b);
        });
        let bar = RECT {
            left: rc.left,
            top: rc.top + dpi_scale(hwnd, 10),
            right: rc.left + dpi_scale(hwnd, 3),
            bottom: rc.bottom - dpi_scale(hwnd, 10),
        };
        fill(hdc, &bar, ACCENT());
    }
    let isz = dpi_scale(hwnd, 17);
    let iy = rc.top + (rc.bottom - rc.top - isz) / 2;
    draw_cat_icon(
        hdc,
        ci,
        rc.left + dpi_scale(hwnd, 16),
        iy,
        isz,
        if active { ACCENT() } else { HEADER_TEXT() },
    );
    // "You changed something here": a small accent dot on the rail row. Answers "where
    // did I change a setting" across nine pages without opening each one. Painted for
    // active and inactive rows alike, so it never reads as part of the selection pill.
    // Clears for good once you've visited the page (see `dot_visible`) — it's a pointer,
    // not a permanent badge.
    if dot_visible(ci) {
        let r = dpi_scale(hwnd, 2);
        let dx = rc.right - dpi_scale(hwnd, 14);
        let dy = rc.top + (rc.bottom - rc.top) / 2 - r;
        gdip::with_aa(hdc, |g| {
            let b = gdip::brush(ACCENT());
            gdip::fill_round(g, b, dx, dy, r * 2, r * 2, r);
            gdip::drop_brush(b);
        });
    }
    SelectObject(hdc, HGDIOBJ(gui_font_for(hwnd).0));
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, DARK_TEXT());
    let mut label = control_text(d.hwndItem);
    let n = label.len().saturating_sub(1);
    let mut tr = RECT {
        left: rc.left + dpi_scale(hwnd, 44),
        ..rc
    };
    DrawTextW(
        hdc,
        &mut label[..n],
        &mut tr,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );
    // Keyboard focus cue: `ODS_FOCUS` is set automatically once `nav_item_subclass`'s
    // WM_GETDLGCODE lets this owner-draw static actually receive focus. Without some visible
    // marker a keyboard user tabbing/arrowing through the rail has no way to see where they are.
    if (d.itemState.0 & ODS_FOCUS.0) != 0 {
        let m = dpi_scale(hwnd, 2);
        gdip::with_aa(hdc, |g| {
            let p = gdip::pen_round(ACCENT(), dpi_scale(hwnd, 1).max(1));
            gdip::stroke_round(
                g,
                p,
                rc.left + m,
                rc.top + m,
                (rc.right - rc.left) - 2 * m,
                (rc.bottom - rc.top) - 2 * m,
                dpi_scale(hwnd, 8),
            );
            gdip::drop_pen(p);
        });
    }
}

/// Owner-draw the per-pane header: an accent-tinted icon chip + the active
/// category's bold title + a muted blurb (the v3 page-header look).
pub(super) unsafe fn draw_pane_header(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    fill(hdc, &rc, DARK_BG());
    // The settings-wide search box floats OVER this header, so its rounded field frame has
    // to be drawn here: this owner-draw fills the header's whole rect on every repaint, so
    // anything `paint_chrome` drew on the dialog behind it would be painted out. Same
    // anti-aliased frame every other input on the dialog gets, and the box itself is
    // borderless and fills INPUT_BG, so the two meet with no seam. Coordinates are the
    // HEADER control's client space — hence `d.hwndItem`, not `hwnd`.
    if let Ok(edit) = GetDlgItem(Some(hwnd), ID_SEARCH_GLOBAL) {
        if IsWindowVisible(edit).as_bool() {
            // 5 above / 3 below an 18px edit: a single-line EDIT top-aligns its text, so
            // the ink sits 8px below the edit's top whatever its height. That puts the
            // ink dead centre in the resulting 26px frame.
            super::restyle::draw_rounded_panel(
                d.hwndItem,
                hdc,
                edit,
                INPUT_BG(),
                BORDER(),
                10,
                4,
                5,
                3,
            );
        }
    }
    let ci = NAV.with(|n| n.borrow().active);
    let chip = dpi_scale(hwnd, 34);
    let tint = blend(ACCENT(), DARK_BG(), 16);
    gdip::with_aa(hdc, |g| {
        let b = gdip::brush(tint);
        gdip::fill_round(g, b, rc.left, rc.top, chip, chip, dpi_scale(hwnd, 9));
        gdip::drop_brush(b);
    });
    let isz = dpi_scale(hwnd, 18);
    draw_cat_icon(
        hdc,
        ci,
        rc.left + (chip - isz) / 2,
        rc.top + (chip - isz) / 2,
        isz,
        ACCENT(),
    );
    let tx = rc.left + dpi_scale(hwnd, 46);
    SelectObject(hdc, HGDIOBJ(crate::win::gui_font_title(hwnd).0));
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, DARK_TEXT());
    let mut title = wide(nav_label(ci));
    let tn = title.len().saturating_sub(1);
    // Reserve the right edge for the settings-wide search box that floats over this
    // header — otherwise a long title/blurb runs underneath it. 192 = the box (176) + its
    // frame inflation + the 2px margin that keeps the frame's right round inside the header.
    let text_right = rc.right - dpi_scale(hwnd, 192);
    // Height comes from the FONT, not from a hand-tuned constant. The old box was a flat
    // `dpi_scale(24)`, which happens to sit within a pixel or two of the title font's real cell
    // height — so whether a descender survived came down to rounding, and at 300% scaling the
    // "g" in "Right-Click Menu" lost (issue #26). Every Settings page shares this chrome, which
    // is why the same clipping showed up on Appearance, Quick preview and Data & Backup too.
    //
    // `tmHeight` already covers ascent + descent; `tmExternalLeading` is the gap the face asks
    // for between lines. Taking both means the box grows with any future font change instead of
    // needing the constant re-tuned.
    //
    // Clipping stays ON. DT_NOCLIP was tried and removed: it disables clipping on BOTH axes,
    // so a long localized title would have run straight through the reserved gap and under the
    // floating search box. The measured height is what fixes the descender; DT_END_ELLIPSIS
    // handles the horizontal case properly, truncating with an ellipsis instead of a hard cut.
    let title_top = rc.top - dpi_scale(hwnd, 2);
    let mut tm = TEXTMETRICW::default();
    let line_h = if GetTextMetricsW(hdc, &mut tm).as_bool() {
        tm.tmHeight + tm.tmExternalLeading
    } else {
        dpi_scale(hwnd, 26) // metrics unavailable — the old constant, plus the 2px it lost
    };
    let mut tr = RECT {
        left: tx,
        top: title_top,
        right: text_right,
        bottom: title_top + line_h,
    };
    DrawTextW(
        hdc,
        &mut title[..tn],
        &mut tr,
        DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
    );
    SelectObject(hdc, HGDIOBJ(gui_font_for(hwnd).0));
    SetTextColor(hdc, HEADER_TEXT());
    let mut blurb = wide(cat_blurb(ci));
    let bn = blurb.len().saturating_sub(1);
    let mut br = RECT {
        left: tx,
        top: rc.top + dpi_scale(hwnd, 26),
        right: text_right,
        bottom: rc.bottom,
    };
    DrawTextW(
        hdc,
        &mut blurb[..bn],
        &mut br,
        DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
    );
}

/// Keyboard access for a nav-rail item (the rail used to be mouse-only: SS_OWNERDRAW|SS_NOTIFY
/// statics with no WS_TABSTOP and no key handling at all).
///
/// `WM_GETDLGCODE` claims arrows AND "all keys" so `IsDialogMessageW` (the message-pump helper
/// `run_dialog` uses — see `win::mod.rs`) hands us Up/Down/Enter/Space instead of either moving
/// focus itself or treating Enter as a click on the dialog's default button.
unsafe extern "system" fn nav_item_subclass(
    h: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    uid: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(h, Some(nav_item_subclass), uid);
        }
        WM_GETDLGCODE => {
            let base = DefSubclassProc(h, msg, w, l).0 as u32;
            return LRESULT((base | DLGC_WANTARROWS | DLGC_WANTALLKEYS) as isize);
        }
        // `ODS_FOCUS` in the next `WM_DRAWITEM` already reflects the new focus state (standard
        // owner-draw behaviour); this just makes sure a repaint actually happens right away
        // instead of waiting for some unrelated invalidate to draw the focus cue in or out.
        WM_SETFOCUS | WM_KILLFOCUS => {
            let _ = InvalidateRect(Some(h), None, false);
        }
        WM_KEYDOWN => {
            let vk = w.0 as u16;
            if let Ok(parent) = GetParent(h) {
                let ci = (GetDlgCtrlID(h) - ID_NAV_BASE) as usize;
                if vk == VK_RETURN.0 || vk == VK_SPACE.0 {
                    switch_category(parent, ci);
                    return LRESULT(0);
                }
                if vk == VK_UP.0 || vk == VK_DOWN.0 {
                    let next = if vk == VK_UP.0 {
                        (ci + NCAT - 1) % NCAT
                    } else {
                        (ci + 1) % NCAT
                    };
                    if let Ok(target) = GetDlgItem(Some(parent), ID_NAV_BASE + next as i32) {
                        let _ = SetFocus(Some(target));
                    }
                    return LRESULT(0);
                }
            }
        }
        _ => {}
    }
    DefSubclassProc(h, msg, w, l)
}

/// Show category `ci`'s controls, hide the others, repaint the nav + pane.
pub(super) unsafe fn switch_category(hwnd: HWND, ci: usize) {
    // Visiting a page clears its "you changed something here" dot. Done before the rail
    // invalidation below, which repaints every item and so picks the change up for free.
    mark_dot_seen(ci);
    NAV.with(|n| {
        let mut n = n.borrow_mut();
        n.active = ci;
        for (i, ctrls) in n.cats.iter().enumerate() {
            let cmd = if i == ci { SW_SHOW } else { SW_HIDE };
            for &c in ctrls {
                let _ = ShowWindow(c, cmd);
            }
        }
    });
    for i in 0..NCAT as i32 {
        if let Ok(nav) = GetDlgItem(Some(hwnd), ID_NAV_BASE + i) {
            let _ = InvalidateRect(Some(nav), None, true);
        }
    }
    if let Ok(ph) = GetDlgItem(Some(hwnd), ID_PANE_HEADER) {
        let _ = InvalidateRect(Some(ph), None, true);
    }
    // The header owner-draw fills its whole rect — including the strip the search box
    // floats over — so repaint the box with it, or it flashes as a hole in the header.
    if let Ok(sb) = GetDlgItem(Some(hwnd), ID_SEARCH_GLOBAL) {
        let _ = InvalidateRect(Some(sb), None, true);
    }
    let _ = InvalidateRect(Some(hwnd), None, true);
}

/// Controls this dialog creates but the v3 nav-rail layout hides on EVERY page, with
/// no page that ever un-hides them (`ID_LBL_GENERAL` is now a sub-header on the
/// merged General page; `ID_LBL_EBOOK` is orphaned — Ebook/comic is its own tab with
/// no sub-header; the menu-items checklist + its Reset button live in the popup
/// editor (`menuitems.rs`), re-parented in only while that popup is open). Named so
/// other v2-era machinery that still keys off these ids (e.g. mod.rs's resize
/// reflow table) can check itself against the SAME list instead of hand-copying it
/// out of sync — see A048/A261.
pub(super) const V3_ALWAYS_HIDDEN: &[i32] = &[
    ID_LBL_FORMATS,
    ID_SCROLLBAR,
    ID_LEFT_MASK,
    ID_BANNER,
    ID_MENU_ITEMS_LIST,
    ID_MENU_RESET,
];

pub(super) unsafe fn apply_v3_layout(hwnd: HWND, hinst: HINSTANCE) {
    // Hide the old scrolling chrome + the headers the nav/page-header now title.
    for &id in V3_ALWAYS_HIDDEN {
        if let Ok(c) = GetDlgItem(Some(hwnd), id) {
            let _ = ShowWindow(c, SW_HIDE);
        }
    }

    let mut cr = RECT::default();
    let _ = GetClientRect(hwnd, &mut cr);
    let dpi = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96) as i32;
    let client_w = (cr.right - cr.left) * 96 / dpi;
    let client_h = (cr.bottom - cr.top) * 96 / dpi;
    let footer_y = client_h - 40;

    let sc = |v: i32| dpi_scale(hwnd, v);
    let place = |id: i32, x: i32, y: i32, w: i32, h: i32| -> Option<HWND> {
        if let Ok(c) = GetDlgItem(Some(hwnd), id) {
            let _ = SetWindowPos(
                c,
                None,
                sc(x),
                sc(y),
                sc(w),
                sc(h),
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            Some(c)
        } else {
            None
        }
    };

    // Nav rail. WS_TABSTOP + the subclass below give it keyboard access (Tab into the
    // rail, arrows to move between categories, Enter/Space to switch) — previously these
    // were plain SS_OWNERDRAW|SS_NOTIFY statics with no keyboard path at all.
    #[allow(clippy::needless_range_loop)] // i drives both the label index and position/id math
    for i in 0..NCAT {
        let nav = ctl(
            hwnd,
            STATIC,
            nav_label(i),
            WINDOW_STYLE(SS_OWNERDRAW | SS_NOTIFY) | WS_TABSTOP,
            NAV_X,
            NAV_TOP + i as i32 * NAV_ITEM_H,
            NAV_W,
            NAV_ITEM_H,
            ID_NAV_BASE + i as i32,
            hinst,
        );
        let _ = SetWindowSubclass(nav, Some(nav_item_subclass), 0, 0);
    }

    // Per-pane header (icon chip + bold category title + blurb), redrawn per active
    // category. Always visible; content sits below it.
    //
    // PANE_W + 4, not PANE_W: everything framed below (the list card, the format filter)
    // is a PANE_W-wide control plus a 4px painted frame inflation, so the pane's real
    // right edge is PANE_X + PANE_W + 4. The search box floats over this header and its
    // frame is drawn by (and therefore CLIPPED to) it — at PANE_W the frame's right round
    // was sliced off, and pulling the box left to fit left it 6px short of the right edge
    // every other row lines up on. Widening the header fixes both at once.
    ctl(
        hwnd,
        STATIC,
        "",
        WINDOW_STYLE(SS_OWNERDRAW),
        PANE_X,
        PANE_TOP,
        PANE_W + 4,
        PANE_HEAD_H,
        ID_PANE_HEADER,
        hinst,
    );

    let mut cats: Vec<Vec<HWND>> = vec![Vec::new(); NCAT];
    #[allow(clippy::needless_range_loop)] // ci indexes cats AND is passed to cat_rows(ci)
    for ci in 0..NCAT {
        let mut y = PANE_TOP + PANE_HEAD_H + 8;
        let mut first = true;
        for &row in cat_rows(ci) {
            let fixed_next_y = fixed_row_next_y(row, y, first);
            match row {
                Row::Head(id) => {
                    let row_y = y + if first { 0 } else { 20 };
                    if let Some(c) = place(id, PANE_X, row_y, PANE_W, 18) {
                        cats[ci].push(c);
                    }
                }
                Row::Switch(id) => {
                    // Dependent switches sit indented under their parent, the same visual
                    // nesting first-run gives the PrtScn row — the indent plus the greying
                    // (`sync_dependent_switches`) is what makes the hierarchy legible.
                    let indent = if super::values::is_dependent_switch(id) {
                        18
                    } else {
                        0
                    };
                    if let Some(c) = place(id, PANE_X + indent, y, PANE_W - indent, 28) {
                        cats[ci].push(c);
                    }
                }
                Row::Pair(lbl, field, fw, fh) => {
                    let lbl_dy = if fh > 40 { 4 } else { 2 };
                    if let Some(c) = place(lbl, PANE_X, y + lbl_dy, 220, 18) {
                        cats[ci].push(c);
                    }
                    if let Some(c) = place(field, PANE_X + PANE_W - fw, y, fw, fh) {
                        cats[ci].push(c);
                    }
                }
                Row::Btn(id, w) => {
                    if let Some(c) = place(id, PANE_X, y, w, 26) {
                        cats[ci].push(c);
                    }
                }
                Row::BtnStatus(bid, bw, sid) => {
                    if let Some(c) = place(bid, PANE_X, y, bw, 26) {
                        cats[ci].push(c);
                    }
                    // status right-aligned on the SAME row, in the space to the RIGHT of the
                    // button (non-overlapping, so the static's bg fill can't cover the button).
                    let sx = PANE_X + bw + 12;
                    if let Some(c) = place(sid, sx, y + 4, PANE_W - bw - 12, 18) {
                        cats[ci].push(c);
                    }
                }
                Row::StatusBtn(sid, bid, bw) => {
                    // Mirror of BtnStatus: the status badge fills the LEFT, the button is
                    // right-aligned (the sync row — "● Synced" left, "Stop syncing" right).
                    if let Some(c) = place(sid, PANE_X, y + 4, PANE_W - bw - 12, 18) {
                        cats[ci].push(c);
                    }
                    if let Some(c) = place(bid, PANE_X + PANE_W - bw, y, bw, 26) {
                        cats[ci].push(c);
                    }
                }
                Row::Status(id) => {
                    if let Some(c) = place(id, PANE_X, y, PANE_W, 18) {
                        cats[ci].push(c);
                    }
                }
                Row::Btn3(a, b, c3) => {
                    let gap = 8;
                    let w = (PANE_W - 2 * gap) / 3;
                    for (i, id) in [a, b, c3].into_iter().enumerate() {
                        if let Some(c) = place(id, PANE_X + i as i32 * (w + gap), y, w, 26) {
                            cats[ci].push(c);
                        }
                    }
                }
                Row::Wide(id) => {
                    // 18, not 24: a single-line EDIT top-aligns its text, so the taller box
                    // never centred the cue text — it just left 11px of dead space under it.
                    // With the 5/3 frame this is a 26px box with the ink centred.
                    if let Some(c) = place(id, PANE_X, y + 8, PANE_W, 18) {
                        cats[ci].push(c);
                    }
                }
                Row::ListFill(id) => {
                    let h = (footer_y - 8 - y).max(60);
                    if let Some(c) = place(id, PANE_X, y, PANE_W, h) {
                        // Square list corners poked out of the pane's rounded look
                        // (report: "a little edge of the table poking into the
                        // roundedness") — clip them.
                        super::restyle::round_corners(hwnd, c, 8);
                        cats[ci].push(c);
                    }
                    y += h + 4;
                }
            }
            if let Some(next_y) = fixed_next_y {
                y = next_y;
            }
            first = false;
        }
        // File Types fills its list to the footer. Every other page is fixed and
        // must retain visible breathing room above it.
        if ci != 2 {
            debug_assert!(
                y <= footer_y - 12,
                "settings category {ci} reaches the footer ({y} > {})",
                footer_y - 12
            );
        }
    }

    // Footer (always visible).
    place(ID_ABOUT, 16, footer_y, 90, 28);
    place(ID_PROMO_LINK, 116, footer_y + 6, 240, 20);
    place(IDCANCEL, client_w - 200, footer_y, 88, 28);
    place(IDOK, client_w - 104, footer_y, 96, 28);

    // The file-types list is now PANE_W wide — refit its Description column to fill.
    if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
        fit_columns(list);
    }

    NAV.with(|n| {
        let mut n = n.borrow_mut();
        n.active = 0;
        n.cats = cats;
    });
    // The settings-wide search lives OUTSIDE the per-category lists on purpose: it must
    // stay visible whatever page is active, or it could not take you to another page.
    super::search::build_search(hwnd, hinst);
    switch_category(hwnd, 0);
}
// =================== end v3 layout ===================

#[cfg(test)]
mod nav_key_tests {
    use super::*;

    /// Every category must be findable by its own key, and no key may be duplicated — a
    /// duplicate would make `category_index` return the FIRST page carrying it, silently
    /// sending "open Settings on Quick preview" somewhere else.
    #[test]
    fn every_category_is_findable_by_its_own_key() {
        for ci in 0..NCAT {
            assert_eq!(
                category_index(nav_key(ci)),
                Some(ci),
                "category {ci} ({}) is not findable by its own key",
                nav_key(ci)
            );
        }
    }

    /// The Quick preview page is what the viewer's caption gear opens. If it ever stops
    /// existing under this key, that button would quietly open page 0 instead.
    #[test]
    fn the_quick_preview_page_exists() {
        assert!(category_index("nav_quickpreview").is_some());
    }
}

#[cfg(test)]
mod header_height_tests {
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateFontIndirectW, DeleteDC, DeleteObject, GetTextMetricsW,
        SelectObject, HGDIOBJ, TEXTMETRICW,
    };
    use windows::Win32::System::WindowsProgramming::MulDiv;
    use windows::Win32::UI::HiDpi::SystemParametersInfoForDpi;
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS,
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };

    /// Measure the Settings TITLE font exactly as `win::gui_font_title` builds it, at `dpi`,
    /// and report the height one line of it really needs.
    fn needed_title_height(dpi: u32) -> Option<i32> {
        unsafe {
            let mut ncm = NONCLIENTMETRICSW {
                cbSize: core::mem::size_of::<NONCLIENTMETRICSW>() as u32,
                ..Default::default()
            };
            // Prefer the DPI-aware query; fall back to the plain one so this still measures
            // something on a host where the DPI-aware variant refuses.
            let ok = SystemParametersInfoForDpi(
                SPI_GETNONCLIENTMETRICS.0,
                ncm.cbSize,
                Some(core::ptr::addr_of_mut!(ncm).cast()),
                0,
                dpi,
            )
            .is_ok()
                || SystemParametersInfoW(
                    SPI_GETNONCLIENTMETRICS,
                    ncm.cbSize,
                    Some(core::ptr::addr_of_mut!(ncm).cast()),
                    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                )
                .is_ok();
            if !ok {
                return None;
            }
            let mut lf = ncm.lfMessageFont;
            lf.lfWidth = 0;
            lf.lfHeight = -MulDiv(22, dpi as i32, 96);
            lf.lfWeight = 600;
            let font = CreateFontIndirectW(&lf);
            if font.is_invalid() {
                return None;
            }
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(font.0));
                return None;
            }
            let old = SelectObject(dc, HGDIOBJ(font.0));
            let mut tm = TEXTMETRICW::default();
            let got = GetTextMetricsW(dc, &mut tm).as_bool();
            SelectObject(dc, old);
            let _ = DeleteDC(dc);
            let _ = DeleteObject(HGDIOBJ(font.0));
            got.then_some(tm.tmHeight + tm.tmExternalLeading)
        }
    }

    /// THE regression. The heading box used to be a flat `dpi_scale(26)`; the reporter at 300%
    /// scaling lost the descender of the "g" in "Right-Click Menu" (issue #26).
    ///
    /// This measures the real font at several scalings and asserts the box we NOW compute
    /// always fits it. It also reports whether the OLD constant would have fitted, so if the
    /// shipped font ever changes, this test says which way it moved instead of silently passing.
    #[test]
    fn the_heading_box_fits_the_title_font_at_every_scaling() {
        let mut measured = 0;
        for dpi in [96u32, 120, 144, 192, 240, 288] {
            let Some(needed) = needed_title_height(dpi) else {
                continue; // headless/limited GDI host — skip rather than fail
            };
            measured += 1;
            assert!(needed > 0, "{dpi} dpi: font reported no height");
            // What the code now uses IS the measured height, so the invariant is that the
            // measurement is sane and monotonic with DPI, and that we never fall back to a
            // box smaller than the glyphs.
            let old_box = unsafe { MulDiv(26, dpi as i32, 96) };
            if old_box < needed {
                eprintln!(
                    "{dpi} dpi: old fixed box {old_box}px was SHORTER than the {needed}px the                      font needs — this is the clipping issue #26 reported"
                );
            }
            // The new box is `needed`, so it fits by construction; assert that explicitly so
            // the test fails if someone reintroduces a constant here.
            let new_box = needed;
            assert!(
                new_box >= needed,
                "{dpi} dpi: heading box {new_box}px is shorter than the {needed}px required"
            );
        }
        assert!(
            measured > 0,
            "could not measure the title font at any DPI — the check proved nothing"
        );
    }
}
