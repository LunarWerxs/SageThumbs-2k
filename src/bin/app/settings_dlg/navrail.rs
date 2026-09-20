//! v3 nav-rail + content-pane layout (extracted from settings_dlg; parent-hub pattern).

use super::*;
mod rows;
use rows::*;
mod dots;
mod draw;
use dots::*;
mod navkeys;
use navkeys::*;
mod place;
use crate::gdip;
use crate::uia;
pub(super) use dots::dot_visible;
pub(super) use draw::{blend, draw_nav_item, draw_pane_header};
pub(super) use place::apply_v3_layout;
#[cfg(test)]
pub(super) use place::V3_ALWAYS_HIDDEN;
pub(super) use rows::{cat_rows, pair_field_ids, wide_edit_ids, Row};
use windows::Win32::Graphics::Gdi::{GetTextMetricsW, DT_END_ELLIPSIS, TEXTMETRICW};
use windows::Win32::UI::Accessibility::{NotifyWinEvent, UIA_ListItemControlTypeId};
// EVENT_OBJECT_SELECTION, OBJID_CLIENT and CHILDID_SELF come from
// `windows::Win32::UI::WindowsAndMessaging`, already glob-imported by `mod.rs` and visible
// here through `use super::*` above. Same for `WM_GETOBJECT`.
use windows::Win32::UI::Controls::ODS_FOCUS;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus, VK_DOWN, VK_RETURN, VK_UP};

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
pub(super) const NCAT: usize = 11;
/// The Licence page's index: the last category, the `_` arm of [`nav_key`]. Named so the code
/// that shows and hides that page's own rows (`licence_ui::apply_conditional_visibility`) can
/// ask "is it the page on screen" instead of re-deriving the arm; `licence_is_the_last_category`
/// pins the two together.
pub(super) const CAT_LICENCE: usize = NCAT - 1;
// ID_NAV_BASE and ID_PANE_HEADER live in ids.rs now (so `control_ids_are_unique` there
// covers them), but the id-space relationship is this module's invariant to keep, so the
// build-time check stays here. The nav ids and ID_PANE_HEADER share one id space, and at
// NCAT = 11 they fit with exactly ZERO headroom: nav owns 1700..=1710 and the header sits
// on 1711. A twelfth category would silently hand the pane header a nav item's identity,
// and the two `(ID_NAV_BASE..ID_NAV_BASE + NCAT)` range tests in `mod.rs`'s WM_COMMAND
// would start routing clicks on the header as a category switch. Nothing about that fails
// to compile or looks wrong in a diff, which is exactly the shape of bug this repo keeps
// paying for, so it fails the BUILD instead.
// (The stale comment this replaces still said the range ended at 1708, from when NCAT was 8,
// then 1709/NCAT=10 when the Licence category — the 11th — was added.)
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
        9 => "nav_databackup",
        _ => "nav_licence",
    }
}

/// Localized nav-rail / page-header label for category `ci`. Pulls from `t()` so a
/// live language switch re-texts it (the nav statics + pane header re-read this).
pub(super) fn nav_label(ci: usize) -> &'static str {
    t(nav_key(ci))
}

/// The big title painted over category `ci`'s content pane: its nav label, except on the Licence
/// page, which names the licence this copy holds (see `licence_page_title`). Kept apart from
/// [`nav_label`] on purpose - the rail and search must keep finding the page by its plain name.
pub(super) fn pane_title(ci: usize) -> &'static str {
    if nav_key(ci) == "nav_licence" {
        return super::licence_page_title(&crate::license::snapshot());
    }
    nav_label(ci)
}

/// The category index whose label key is `key`, or `None` if no page carries it.
pub(super) fn category_index(key: &str) -> Option<usize> {
    (0..NCAT).find(|&ci| nav_key(ci) == key)
}

#[derive(Default)]
pub(super) struct NavState {
    pub(super) active: usize,
    pub(super) cats: Vec<Vec<HWND>>,
}
thread_local! {
    pub(super) static NAV: std::cell::RefCell<NavState> = std::cell::RefCell::new(NavState::default());
}

/// Show category `ci`'s controls and hide every other category's.
unsafe fn set_active_category_controls(ci: usize) {
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
}

/// Repaint every nav row, the pane header and the settings-wide search box, then the dialog
/// (the header owner-draw fills its whole rect — including the strip the search box floats
/// over — so the box must repaint with it, or it flashes as a hole in the header).
unsafe fn invalidate_nav_chrome(hwnd: HWND) {
    for i in 0..NCAT as i32 {
        if let Ok(nav) = GetDlgItem(Some(hwnd), ID_NAV_BASE + i) {
            let _ = InvalidateRect(Some(nav), None, true);
        }
    }
    if let Ok(ph) = GetDlgItem(Some(hwnd), ID_PANE_HEADER) {
        let _ = InvalidateRect(Some(ph), None, true);
    }
    if let Ok(sb) = GetDlgItem(Some(hwnd), ID_SEARCH_GLOBAL) {
        let _ = InvalidateRect(Some(sb), None, true);
    }
    let _ = InvalidateRect(Some(hwnd), None, true);
}

/// Show category `ci`'s controls, hide the others, repaint the nav + pane.
pub(super) unsafe fn switch_category(hwnd: HWND, ci: usize) {
    // Visiting a page clears its "you changed something here" dot. Done before the rail
    // invalidation below, which repaints every item and so picks the change up for free.
    mark_dot_seen(ci);
    set_active_category_controls(ci);
    // The blanket show above does not know that some controls hide themselves. Anything
    // conditionally visible has to re-decide right here, or navigating away and back is all
    // it takes to reveal a row the page had deliberately hidden.
    licence_ui::apply_conditional_visibility(hwnd);
    invalidate_nav_chrome(hwnd);
    // Tell assistive tech the active nav item changed: the owner-draw rail never fires
    // WM_GETOBJECT/selection notifications on its own, so a screen reader has no way to
    // know which page is now current without this. Two events, not one: `NotifyWinEvent`
    // is the legacy MSAA path (closed G199, the narrower report — kept, not replaced), and
    // `raise_selection_changed` is the UIA path this module adds; a client listening on
    // only one of the two automation stacks must still hear it.
    if let Ok(nav) = GetDlgItem(Some(hwnd), ID_NAV_BASE + ci as i32) {
        NotifyWinEvent(
            EVENT_OBJECT_SELECTION,
            nav,
            OBJID_CLIENT.0,
            CHILDID_SELF as i32,
        );
        uia::raise_selection_changed(nav, &NAV_ITEM_UIA_OPS);
    }
}

// =================== end v3 layout ===================

#[cfg(test)]
mod header_height_tests;
#[cfg(test)]
mod nav_key_tests;
#[cfg(test)]
mod pair_field_ids_tests;
#[cfg(test)]
mod uia_tests;
