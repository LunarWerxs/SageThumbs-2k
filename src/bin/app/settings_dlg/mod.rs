//! The main Settings window — a faithful, modernized port of the original
//! SageThumbs Options dialog. Edits HKCU\Software\SageThumbs2K via the crate's
//! `settings` module, plus a per-format checkbox list (a ListView). Built
//! programmatically (CreateWindowExW) rather than from a dialog-template resource.
//!
//! Reachable settings take effect immediately (the provider reads them per
//! request). Changing the per-format list rewrites the HKCR `shellex` keys, which
//! needs elevation — handled by re-running `regsvr32` elevated.

use core::ffi::c_void;

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC,
    DeleteObject, DrawTextW, EndPaint, FillRect, GetDC,
    GetStockObject, GetTextExtentPoint32W, InvalidateRect, RedrawWindow, ReleaseDC,
    ScreenToClient, SelectObject, SetBkMode, SetDCBrushColor,
    SetTextCharacterExtra, SetTextColor, SetViewportOrgEx, DC_BRUSH, DT_CENTER,
    DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HBRUSH, HDC, HGDIOBJ,
    PAINTSTRUCT, RDW_ALLCHILDREN, RDW_INVALIDATE, RDW_UPDATENOW, SRCCOPY, TRANSPARENT,
};
use windows::Win32::UI::Controls::{
    CDDS_ITEMPOSTPAINT, CDDS_ITEMPREPAINT, CDDS_PREPAINT, CDDS_SUBITEM, CDIS_FOCUS, CDIS_HOT,
    CDIS_SELECTED, CDRF_DODEFAULT, CDRF_NEWFONT, CDRF_NOTIFYITEMDRAW, CDRF_NOTIFYPOSTPAINT,
    CDRF_NOTIFYSUBITEMDRAW, CDRF_SKIPDEFAULT,
    LVCFMT_LEFT, LVCF_FMT, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVIF_TEXT, LVIS_STATEIMAGEMASK,
    LVIF_PARAM, LVIF_STATE, LVM_DELETEALLITEMS, LVM_GETITEMCOUNT,
    LVITEMW, LVM_GETHEADER, LVM_GETITEMRECT, LVM_GETITEMSTATE, LVM_GETNEXTITEM, LVM_GETSELECTEDCOUNT,
    LVM_INSERTCOLUMNW, LVM_INSERTITEMW, LVM_SETBKCOLOR, LVM_SETCOLUMNW, LVM_SETCOLUMNWIDTH,
    LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMSTATE, LVM_SETITEMW, LVN_ITEMCHANGED, NMLISTVIEW,
    LVM_SETTEXTBKCOLOR, LVM_SETTEXTCOLOR, LVNI_FOCUSED, LVNI_SELECTED, LVS_EX_CHECKBOXES,
    LVS_EX_FULLROWSELECT,
    LVS_NOCOLUMNHEADER, LVS_NOSORTHEADER, LVS_REPORT, LIST_VIEW_ITEM_STATE_FLAGS, DRAWITEMSTRUCT, MEASUREITEMSTRUCT,
    NMCUSTOMDRAW, NMHDR, NMLINK, NMLVCUSTOMDRAW, NM_CLICK, NM_CUSTOMDRAW, NM_RETURN, ODS_SELECTED,
    ODT_MENU, ODT_STATIC, SetScrollInfo, WC_LISTVIEWW, TTTOOLINFOW, TTF_IDISHWND, TTF_SUBCLASS,
    TTM_ADDTOOLW, TTM_POP, TTM_SETMAXTIPWIDTH, NMTTDISPINFOW, TTN_GETDISPINFOW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_SPACE;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::*;

use sagethumbs2k_core::{default_menu_tokens, formats, i18n, settings, MENU_SEP_TOKEN};

use crate::sponsors::{
    drop_sponsor_rotator, show_current_image, spawn_remote_sponsors, sponsors_enabled,
    SponsorRotator, BANNER_PNG, TIMER_BANNER, TIMER_ROTATE, WM_APP_SPONSORS,
};
use crate::about::show_about;
use crate::dark::{
    dark_bg_brush, dark_control, dark_ctlcolor, dark_menu_brush, dark_menu_sel_brush,
    dark_theme_combo, is_dark,
    ACCENT, ACCENT_HOT, ACCENT_PRESS, ACCENT_TEXT, BORDER, BORDER_STRONG, BTN_FACE, BTN_FACE_HOT,
    BTN_FACE_PRESS, CHECK_BG, DARK_BG, DARK_TEXT, DISABLED_TEXT, HEADER_TEXT, INPUT_BG, ON_ACCENT,
    SEL_BG, SURFACE, ZEBRA,
};
use crate::win::{
    check, checked, ctl, dpi_scale, get_edit_text, gui_font, gui_font_for, gui_font_header, load_art,
    message_box, open_url, set_static_bitmap, t, wide, wm_dpichanged, wstr_to_string, BTN_H, BUTTON,
    COMBOBOX, EDIT, STATIC, SYSLINK, CHECKED, EDIT_X, IDCANCEL, IDOK, INDENT, LABEL_W, MARGIN,
    SS_BITMAP, SS_NOTIFY, SS_OWNERDRAW, SS_REALSIZECONTROL,
    TTS_ALWAYSTIP, TTS_NOPREFIX, UNCHECKED, URL_PARENT, URL_PRODUCT,
};

// Submodules split out of this (formerly ~2030-line) file. They're descendants of
// this module, so they freely call its private helpers via `super::` (s, fill,
// control_text, set_check, is_checked, …); the parent reaches their entry points
// via the module path (restyle::…, scroll::…, list::…).
mod restyle; // dark-mode owner-draw painting + the combo/scrollbar subclasses
mod scroll; // the left-column scroll subsystem (incl. its clipping mask)
mod list; // the self-contained ListView subclass + bulk-toggle context menu

mod ids;
pub(super) use ids::*;
mod build;
use build::*;
mod navrail;
mod localize;
mod values;
mod sync;
mod helpers;
mod shot;
use navrail::*;
use localize::*;
use values::*;
use sync::*;
use helpers::*;
pub(crate) use shot::{run_shot, run_shot_gif};
// Win32 message consts the `windows` crate omits (local so they shadow the `WindowsAndMessaging::*` glob).
const EM_SETCUEBANNER: u32 = 0x1501;
const CB_SETDROPPEDWIDTH: u32 = 0x0160;

#[derive(Clone, Copy)]
pub(super) struct SponsorLayout {
    banner_y: i32,
    foot_y: i32,
    credit_y: i32,
}

pub(super) fn sponsor_layout(_dark: bool, sponsors_on: bool) -> SponsorLayout {
    // Compact layout in BOTH themes — light is a recolored clone of dark, so the
    // left column scrolls and the window stays short regardless of theme.
    let banner_y = 460;
    let foot_y = if sponsors_on { 534 } else { 470 };
    SponsorLayout { banner_y, foot_y, credit_y: foot_y + 6 }
}

// Left-column vertical rhythm (96-dpi design px). These are TOP MARGINS — the gap
// ABOVE each control, keyed to its type — so a dropdown always gets more breathing
// room above it than a checkbox, regardless of what precedes it (a control's spacing
// shouldn't depend on the previous row's type). The cursor adds the margin, places
// the control, then advances by the control's own height. EVERY left-column control
// goes through a LeftCol method (header/checkbox/edit/combo/checklist/button/status)
// so the rhythm is uniform — retune the whole column HERE, never via individual y's.
// Control heights: header 18, checkbox 20, edit 18, combo 23, button 24, status 18.
const MT_SECTION: i32 = 20; // above a (non-first) section header
const MT_CHECK: i32 = 6; // above a checkbox / status line (compact rhythm)
const MT_FIELD: i32 = 14; // above a label+combo / label+edit (roomier than a checkbox)
const MT_BUTTON: i32 = 12; // above a push button (an action — between a checkbox and a field)

/// A top-to-bottom layout cursor for the scrolling left options column. Each call
/// drops a control at the running `y`, then advances `y` by the type-based amount
/// above — so spacing stays uniform no matter how the sections are reordered.
pub(super) struct LeftCol {
    hwnd: HWND,
    hinst: HINSTANCE,
    y: i32,
}

impl LeftCol {
    fn new(hwnd: HWND, hinst: HINSTANCE) -> Self {
        Self { hwnd, hinst, y: 12 }
    }

    /// Section header (uppercase label + divider in dark). `first` skips the leading
    /// section gap (the topmost header sits at the column's start).
    unsafe fn header(&mut self, text: &str, style: WINDOW_STYLE, id: i32, first: bool) {
        self.y += if first { 0 } else { MT_SECTION };
        ctl(self.hwnd, STATIC, text, style, MARGIN, self.y, 322, 18, id, self.hinst);
        self.y += 18;
    }

    /// A full-width checkbox row. Kept compact (20px tall, small lead gap) so the
    /// stack of left-column options stays short.
    unsafe fn checkbox(&mut self, text: &str, style: WINDOW_STYLE, w: i32, id: i32) {
        self.y += MT_CHECK;
        ctl(self.hwnd, BUTTON, text, style, INDENT, self.y, w, 20, id, self.hinst);
        self.y += 20;
    }

    /// `label:` + a right-aligned numeric edit; returns the edit hwnd. The label is
    /// dropped 1px to sit against the field. `lbl_id` keeps it live-retranslatable AND
    /// tooltip-targetable, and `SS_NOTIFY` makes it mouse-receptive so a hover tooltip on
    /// the label fires (a plain static is click-through, so its hint never showed).
    unsafe fn edit(&mut self, label: &str, lbl_id: i32, style: WINDOW_STYLE, id: i32) -> HWND {
        self.y += MT_FIELD;
        ctl(self.hwnd, STATIC, label, WINDOW_STYLE(SS_NOTIFY), INDENT, self.y + 1, LABEL_W, 18, lbl_id, self.hinst);
        let e = ctl(self.hwnd, EDIT, "", style, EDIT_X, self.y, 84, 18, id, self.hinst);
        self.y += 18;
        e
    }

    /// `label:` + a dropdown combo at x=160; returns the combo hwnd for the caller to
    /// fill + theme. `lbl_id` keeps the label live-retranslatable.
    unsafe fn combo(&mut self, label: &str, lbl_id: i32, drop_h: i32, id: i32) -> HWND {
        self.y += MT_FIELD;
        ctl(self.hwnd, STATIC, label, WINDOW_STYLE(0), INDENT, self.y + 4, 130, 18, lbl_id, self.hinst);
        let c = ctl(
            self.hwnd,
            COMBOBOX,
            "",
            WINDOW_STYLE(CBS_DROPDOWNLIST as u32) | WS_VSCROLL | WS_TABSTOP,
            160,
            self.y,
            156,
            drop_h,
            id,
            self.hinst,
        );
        self.y += 23;
        c
    }

    /// A full-width, fixed-height checkbox ListView (the "Menu items" checklist) —
    /// one compact card instead of a tall stack of checkboxes, mirroring the
    /// Supported File Types list's dark styling. Caller inserts the single column +
    /// rows. Returns its hwnd.
    unsafe fn checklist(&mut self, h: i32, id: i32) -> HWND {
        self.y += MT_CHECK;
        let base = LVS_REPORT | LVS_NOSORTHEADER | LVS_NOCOLUMNHEADER;
        let style = WINDOW_STYLE(base) | WS_TABSTOP;
        let list = ctl(self.hwnd, WC_LISTVIEWW, "", style, MARGIN, self.y, 322, h, id, self.hinst);
        SendMessageW(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            Some(WPARAM(0)),
            Some(LPARAM((LVS_EX_CHECKBOXES | LVS_EX_FULLROWSELECT) as isize)),
        );
        // Theme the list surface (SURFACE()/DARK_TEXT() are theme-aware). NOT
        // applying DarkMode_Explorer in either theme — it gives dark check glyphs +
        // a scrollbar that vanishes on the surface.
        SendMessageW(list, LVM_SETBKCOLOR, None, Some(LPARAM(SURFACE().0 as isize)));
        SendMessageW(list, LVM_SETTEXTBKCOLOR, None, Some(LPARAM(SURFACE().0 as isize)));
        SendMessageW(list, LVM_SETTEXTCOLOR, None, Some(LPARAM(DARK_TEXT().0 as isize)));
        // Reuse the format list's subclass (SPACE bulk-toggle; header custom-draw is
        // a no-op with no header).
        let _ = SetWindowSubclass(list, Some(list::list_subclass), 0, 0);
        self.y += h;
        list
    }

    /// A push-button action row (e.g. Restart hotkey service / Open diagnostics log) —
    /// `INDENT`-aligned, fixed 24px tall, with a button-sized top margin. Advances the
    /// cursor past the button so the NEXT section header isn't crowded.
    unsafe fn button(&mut self, text: &str, w: i32, id: i32) {
        self.y += MT_BUTTON;
        ctl(self.hwnd, BUTTON, text, WS_TABSTOP, INDENT, self.y, w, 24, id, self.hinst);
        self.y += 24;
    }

    /// A row of equal-width push buttons sharing ONE line — so the Reset / Import /
    /// Export trio fits on a single row instead of three stacked rows. Spans the full
    /// column-content width (like `header`) with small gaps, and advances the cursor once.
    unsafe fn button_row(&mut self, buttons: &[(&str, i32)]) {
        self.y += MT_BUTTON;
        let n = buttons.len() as i32;
        if n > 0 {
            // Narrower than the full column width so the rightmost button clears the
            // left-column scrollbar on its right (it was overrunning into it).
            const FULL_W: i32 = 300;
            const GAP: i32 = 6;
            let w = (FULL_W - GAP * (n - 1)) / n;
            for (i, &(text, id)) in buttons.iter().enumerate() {
                let x = MARGIN + i as i32 * (w + GAP);
                ctl(self.hwnd, BUTTON, text, WS_TABSTOP, x, self.y, w, 24, id, self.hinst);
            }
        }
        self.y += 24;
    }

    /// A single line of dynamic status text (e.g. the hotkey-service state), empty at
    /// build time and filled later via SetDlgItemText. Checkbox-tight gap above.
    unsafe fn status(&mut self, id: i32) {
        self.y += MT_CHECK;
        ctl(self.hwnd, STATIC, "", WINDOW_STYLE(0), INDENT, self.y + 2, 300, 18, id, self.hinst);
        self.y += 18;
    }
}

/// Parse `tokens` (item keys + `verbs::MENU_SEP_TOKEN` divider markers) into menu-list
/// rows `(lParam, checked)`: an item row carries its `MENU_ITEM_TOGGLES` index + its
/// `check(index)` state; a divider token becomes a `list::SEP_PARAM` row. Items are
/// de-duped and any missing from a stale order are appended in default order, so every
/// toggle appears exactly once.
pub(super) fn menu_rows_from_tokens(tokens: &[String], check: impl Fn(usize) -> bool) -> Vec<(isize, bool)> {
    let mut rows = Vec::with_capacity(tokens.len() + MENU_ITEM_TOGGLES.len());
    let mut seen = vec![false; MENU_ITEM_TOGGLES.len()];
    for tok in tokens {
        if tok == MENU_SEP_TOKEN {
            rows.push((list::SEP_PARAM, false));
        } else if let Some(i) = MENU_ITEM_TOGGLES.iter().position(|(_, k)| *k == tok.as_str()) {
            if !seen[i] {
                seen[i] = true;
                rows.push((i as isize, check(i)));
            }
        }
    }
    for (i, &shown) in seen.iter().enumerate() {
        if !shown {
            rows.push((i as isize, check(i)));
        }
    }
    // Show the list in the SAME normalized form the menu renders (no double/edge
    // dividers), so a saved order with a stray double loads cleanly + mirrors the menu.
    list::normalize_rows(&rows)
}

/// Menu-list rows for the CURRENT saved order (or the factory order if none saved), each
/// item's checkbox seeded from its saved visibility.
pub(super) fn saved_menu_rows() -> Vec<(isize, bool)> {
    let saved = settings::menu_order();
    let tokens: Vec<String> = if saved.is_empty() {
        default_menu_tokens().iter().map(|s| s.to_string()).collect()
    } else {
        saved
    };
    menu_rows_from_tokens(&tokens, |i| settings::menu_item_shown(MENU_ITEM_TOGGLES[i].1))
}

/// The factory (default) menu-list rows — items + dividers in tree order, each item
/// checked per `check`. Backs "Reset order" (current checks) and "Defaults" (all on).
pub(super) fn default_menu_rows(check: impl Fn(usize) -> bool) -> Vec<(isize, bool)> {
    let tokens: Vec<String> = default_menu_tokens().iter().map(|s| s.to_string()).collect();
    menu_rows_from_tokens(&tokens, check)
}

/// The toggle index stored in a menu-list row's `lParam` (its `MENU_ITEM_TOGGLES`
/// index), or None if the row/param is out of range. Lets load/save map row→key
/// after the rows have been drag-reordered.
pub(super) unsafe fn menu_row_toggle(list: HWND, row: i32) -> Option<usize> {
    let mut item = LVITEMW {
        mask: windows::Win32::UI::Controls::LVIF_PARAM,
        iItem: row,
        ..Default::default()
    };
    let ok = SendMessageW(
        list,
        windows::Win32::UI::Controls::LVM_GETITEMW,
        Some(WPARAM(0)),
        Some(LPARAM(&mut item as *mut _ as isize)),
    );
    let ti = item.lParam.0 as usize;
    (ok.0 != 0 && ti < MENU_ITEM_TOGGLES.len()).then_some(ti)
}

/// The raw `lParam` of a menu-list row (a toggle index, or `list::SEP_PARAM` for a
/// divider row); `isize::MIN` if the row can't be read. Lets save distinguish divider
/// rows from item rows after a drag-reorder.
pub(super) unsafe fn menu_row_param(list: HWND, row: i32) -> isize {
    let mut item = LVITEMW {
        mask: windows::Win32::UI::Controls::LVIF_PARAM,
        iItem: row,
        ..Default::default()
    };
    let ok = SendMessageW(
        list,
        windows::Win32::UI::Controls::LVM_GETITEMW,
        Some(WPARAM(0)),
        Some(LPARAM(&mut item as *mut _ as isize)),
    );
    if ok.0 != 0 {
        item.lParam.0
    } else {
        isize::MIN
    }
}


/// (control id, hint locale key) for every tooltip. Shared by `add_tooltips`
/// (initial install) and `refresh_tooltips` (re-translate on a live language
/// change). The banner's hint is dynamic (rotates with the ad) so it's excluded
/// here and pulled via a TTN_GETDISPINFO callback instead.
const TOOLTIPS: &[(i32, &str)] = &[
    (ID_ENABLE_THUMBS, "tip_enable_thumbs"),
    (ID_USE_EMBEDDED, "tip_prefer_embedded"),
    (ID_ENABLE_MENU, "tip_enable_menu"),
    (ID_MENU_PREVIEW, "tip_menu_preview"),
    (ID_MENU_QUICK, "tip_menu_quick"),
    (ID_MENU_CHECKER, "tip_menu_checker"),
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
    (ID_PREVIEW_MD_REMOTE, "tip_preview_md_remote"),
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
    (ID_UPDATE_AUTO, "tip_update_auto"),
    (ID_CHECK_UPDATES, "tip_check_updates"),
    (ID_RESET_ALL, "tip_reset_all"),
    (ID_IMPORT, "tip_import"),
    (ID_EXPORT, "tip_export"),
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
        let Ok(ctl) = GetDlgItem(Some(hwnd), id) else { continue };
        // comctl32 copies the text on TTM_ADDTOOL, so this buffer can be temporary.
        let text = wide(t(key));
        let mut ti = TTTOOLINFOW {
            cbSize: core::mem::size_of::<TTTOOLINFOW>() as u32,
            uFlags: TTF_IDISHWND | TTF_SUBCLASS,
            hwnd,
            uId: ctl.0 as usize,
            lpszText: PWSTR(text.as_ptr() as *mut u16),
            ..Default::default()
        };
        SendMessageW(tip, TTM_ADDTOOLW, Some(WPARAM(0)), Some(LPARAM(&mut ti as *mut _ as isize)));
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
        SendMessageW(tip, TTM_ADDTOOLW, Some(WPARAM(0)), Some(LPARAM(&mut ti as *mut _ as isize)));
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
        let Ok(ctl) = GetDlgItem(Some(hwnd), id) else { continue };
        let text = wide(t(key));
        let mut ti = TTTOOLINFOW {
            cbSize: core::mem::size_of::<TTTOOLINFOW>() as u32,
            uFlags: TTF_IDISHWND,
            hwnd,
            uId: ctl.0 as usize,
            lpszText: PWSTR(text.as_ptr() as *mut u16),
            ..Default::default()
        };
        SendMessageW(tip, TTM_UPDATETIPTEXTW, Some(WPARAM(0)), Some(LPARAM(&mut ti as *mut _ as isize)));
    }
}

/// Insert one ListView report column.
pub(super) unsafe fn insert_column(list: HWND, idx: i32, title: &str, cx: i32) {
    let t = wide(title);
    let mut col = LVCOLUMNW {
        mask: LVCF_FMT | LVCF_WIDTH | LVCF_TEXT,
        fmt: LVCFMT_LEFT,
        cx,
        pszText: PWSTR(t.as_ptr() as *mut u16),
        ..Default::default()
    };
    SendMessageW(list, LVM_INSERTCOLUMNW, Some(WPARAM(idx as usize)), Some(LPARAM(&mut col as *mut _ as isize)));
}

/// Set a ListView subitem's text (Category / Description columns).
pub(super) unsafe fn set_subitem(list: HWND, row: i32, col: i32, text: &str) {
    let w = wide(text);
    let sub = LVITEMW {
        mask: LVIF_TEXT,
        iItem: row,
        iSubItem: col,
        pszText: PWSTR(w.as_ptr() as *mut u16),
        ..Default::default()
    };
    SendMessageW(list, LVM_SETITEMW, Some(WPARAM(0)), Some(LPARAM(&sub as *const _ as isize)));
}

// ---- File-types list model + filter ------------------------------------
// The per-format checked state is the source of truth (FMT_STATE), so the search
// can rebuild the list view without losing toggles. Each list row stashes its
// FORMATS index in its lParam; the LVN_ITEMCHANGED handler syncs FMT_STATE back.

thread_local! {
    static FMT_STATE: core::cell::RefCell<Vec<bool>> = const { core::cell::RefCell::new(Vec::new()) };
    static POPULATING: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    /// Last normalized search needle the list was rebuilt for — lets the EN_CHANGE
    /// handler skip an identical rebuild. Cleared on a live language change (rows may
    /// re-localize) so the next search re-filters.
    static LAST_FILTER: core::cell::RefCell<Option<String>> = const { core::cell::RefCell::new(None) };
    /// GDI+ token for this window's lifetime — started in `WM_CREATE`, shut down in
    /// `WM_DESTROY`. GDI+ must be live on the thread before the anti-aliased owner-draw
    /// (toggle switches, checkbox glyphs, nav icons, rounded buttons) can render.
    static GDIP_TOKEN: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

/// RAII guard: clears POPULATING on scope exit (even on unwind), so the
/// LVN_ITEMCHANGED → FMT_STATE sync can never be left silently disabled.
pub(super) struct PopulateGuard;
impl Drop for PopulateGuard {
    fn drop(&mut self) {
        POPULATING.with(|p| p.set(false));
    }
}

/// Rebuild the list to show the formats matching `filter` (extension / category /
/// description, case-insensitive; empty = all), each row's checkbox from FMT_STATE.
pub(super) unsafe fn populate_list(list: HWND, filter: &str) {
    let needle = filter.trim().to_lowercase();
    // Snapshot the model so the LVN_ITEMCHANGED handler (which borrows FMT_STATE)
    // can't clash with the set_check calls below; POPULATING also suppresses it.
    let state: Vec<bool> = FMT_STATE.with(|s| s.borrow().clone());
    POPULATING.with(|p| p.set(true));
    let _guard = PopulateGuard; // resets POPULATING on exit
    SendMessageW(list, LVM_DELETEALLITEMS, None, None);
    let mut row = 0i32;
    for (i, &(ext, desc)) in formats::FORMATS.iter().enumerate() {
        let cat = formats::category_label(formats::category(ext));
        if !needle.is_empty() {
            let hay = format!(".{ext} {cat} {desc}").to_lowercase();
            if !hay.contains(&needle) {
                continue;
            }
        }
        let elabel = wide(&format!(".{ext}"));
        let mut item = LVITEMW {
            mask: LVIF_TEXT | LVIF_PARAM,
            iItem: row,
            iSubItem: 0,
            pszText: PWSTR(elabel.as_ptr() as *mut u16),
            lParam: LPARAM(i as isize),
            ..Default::default()
        };
        SendMessageW(list, LVM_INSERTITEMW, Some(WPARAM(0)), Some(LPARAM(&mut item as *mut _ as isize)));
        set_subitem(list, row, 1, cat);
        set_subitem(list, row, 2, desc);
        set_check(list, row, *state.get(i).unwrap_or(&false));
        row += 1;
    }
    fit_columns(list);
}

/// Size the Description column to fill the list's current visible width — no dead
/// gap, no horizontal scroll. Re-run after a filter (the scrollbar may toggle).
pub(super) unsafe fn fit_columns(list: HWND) {
    let mut crc = RECT::default();
    let _ = GetClientRect(list, &mut crc);
    // 64 + 92 are the extension + category column widths.
    let descw = ((crc.right - crc.left) - 64 - 92).max(80);
    SendMessageW(list, LVM_SETCOLUMNWIDTH, Some(WPARAM(2)), Some(LPARAM(descw as isize)));
}

pub(super) unsafe fn set_shot_status(hwnd: HWND, txt: &str) {
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_SHOT_STATUS) {
        let w = wide(txt);
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
    }
}

/// Update the save-folder display (ID_SHOT_DIR) to the effective folder (the configured
/// one, or the Desktop default). Called on load and after the folder picker.
pub(super) unsafe fn set_shot_dir_label(hwnd: HWND) {
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_SHOT_DIR) {
        let w = wide(&format!("Folder: {}", crate::screenshot::effective_save_dir()));
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
    }
}

/// Refresh the screenshot daemon status line from the live state.
pub(super) unsafe fn refresh_shot_status(hwnd: HWND) {
    use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
    // Drive off the LIVE "Enable screenshot hotkey" checkbox (not the persisted state),
    // so toggling it updates the status line + Restart button immediately.
    let enabled = checked(hwnd, ID_SHOT_ENABLE);
    // The daemon reports per-chord RegisterHotKey failures via the HotkeyBindFailed
    // bitmask (bit0 capture, bit1 quick-save, bit2 custom action) — a chord grabbed by
    // another app otherwise looked identical to a working one ("Running" while the
    // hotkey silently never fires). Only trust the flag while the daemon is actually
    // alive (it rewrites the mask on every re-arm; a dead daemon's value is stale).
    let bind_failed = if crate::screenshot::is_daemon_running() {
        settings::get_dword_opt("HotkeyBindFailed").unwrap_or(0)
    } else {
        0
    };
    let txt = if !enabled {
        // Screenshot feature off — but a bound CUSTOM action hotkey still runs through
        // the same daemon, and ITS conflict (bit2) would otherwise be invisible in the
        // whole UI (this is the only status line).
        if bind_failed & 4 != 0 {
            "Off \u{2014} custom hotkey in use by another app (pick a different one)"
        } else {
            "Off"
        }
    } else if crate::screenshot::is_daemon_running() {
        // Keep the "Running" prefix: the balloon-nudge logic below string-matches it.
        if bind_failed != 0 {
            "Running \u{2014} a hotkey is in use by another app (pick a different one)"
        } else {
            "Running"
        }
    } else {
        "Stopped \u{2014} click Restart"
    };
    set_shot_status(hwnd, txt);
    // The Restart button does nothing when the hotkey is off — disable + repaint it.
    if let Ok(btn) = GetDlgItem(Some(hwnd), ID_SHOT_RESTART) {
        let _ = EnableWindow(btn, enabled);
        let _ = InvalidateRect(Some(btn), None, true);
    }
}

// ---- Vertical resize: let the user drag the window taller --------------------------
// The window grows in HEIGHT only (width locked in WM_GETMINMAXINFO). On WM_SIZE the
// bottom-anchored controls slide down / the stretchy ones grow, and the left scroll
// viewport recomputes — so a taller window simply shows more options at once.

pub(super) struct ReflowCtl {
    id: i32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    stretchy: bool, // true = grow height (top fixed); false = bottom chrome (slide y down)
}
pub(super) struct ResizeState {
    win_w: i32,     // locked window width (device px)
    win_h0: i32,    // minimum window height (device px) = the design size
    client_h0: i32, // original client height (device px), for the resize delta
    ctrls: Vec<ReflowCtl>,
}
thread_local! {
    static RESIZE: core::cell::RefCell<Option<ResizeState>> = const { core::cell::RefCell::new(None) };
}

/// Controls reflowed on resize: the right file-types list + the left scrollbar GROW in
/// height; the fold-mask, sponsor banner, and footer buttons slide down with the bottom.
const REFLOW_CTLS: &[(i32, bool)] = &[
    (ID_LIST, true),
    (ID_SCROLLBAR, true),
    (ID_LEFT_MASK, false),
    (ID_BANNER, false),
    (ID_ABOUT, false),
    (ID_PROMO_LINK, false),
    (IDCANCEL, false),
    (IDOK, false),
];

/// Reflow the bottom-anchored controls for the new client height + recompute the left
/// scroll viewport. The first call (during creation) just captures the design layout.
pub(super) unsafe fn on_resize(hwnd: HWND, client_h: i32) {
    let first = RESIZE.with(|s| s.borrow().is_none());
    if first {
        let mut wr = RECT::default();
        let _ = GetWindowRect(hwnd, &mut wr);
        let mut ctrls = Vec::new();
        for &(id, stretchy) in REFLOW_CTLS {
            if let Ok(h) = GetDlgItem(Some(hwnd), id) {
                let mut r = RECT::default();
                if GetWindowRect(h, &mut r).is_ok() {
                    let mut tl = POINT { x: r.left, y: r.top };
                    let _ = ScreenToClient(hwnd, &mut tl);
                    ctrls.push(ReflowCtl {
                        id,
                        x: tl.x,
                        y: tl.y,
                        w: r.right - r.left,
                        h: r.bottom - r.top,
                        stretchy,
                    });
                }
            }
        }
        RESIZE.with(|s| {
            *s.borrow_mut() = Some(ResizeState {
                win_w: wr.right - wr.left,
                win_h0: wr.bottom - wr.top,
                client_h0: client_h,
                ctrls,
            });
        });
        return; // the first size IS the design layout — nothing to reflow yet
    }
    RESIZE.with(|s| {
        let s = s.borrow();
        let Some(st) = s.as_ref() else { return };
        let delta = client_h - st.client_h0;
        for c in &st.ctrls {
            let Ok(h) = GetDlgItem(Some(hwnd), c.id) else { continue };
            if c.stretchy {
                let _ = SetWindowPos(h, None, 0, 0, c.w, (c.h + delta).max(1), SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
            } else {
                let _ = SetWindowPos(h, None, c.x, c.y + delta, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            }
        }
    });
    scroll::recompute_scroll(hwnd);
    // The grown viewport / moved chrome need a repaint (the mask + dividers).
    let _ = InvalidateRect(Some(hwnd), None, true);
}

pub(crate) extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        // The Quick-save hotkey label stays ENABLED (a disabled static draws an
        // etched/blurry look in dark mode) but reads as greyed when instant
        // screenshot is off — paint its text dim here instead of the normal color.
        if msg == windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
            && GetDlgItem(Some(hwnd), ID_LBL_SHOT_QUICK_HK).is_ok_and(|l| l.0 as isize == lparam.0)
            && !checked(hwnd, ID_SHOT_QUICK_ENABLE)
        {
            return crate::dark::dark_ctlcolor_dim(wparam);
        }
        // The save-folder display greys with the "Save to a set folder" toggle (same as
        // the quick-hotkey label — a disabled static draws etched in dark mode).
        if msg == windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
            && GetDlgItem(Some(hwnd), ID_SHOT_DIR).is_ok_and(|l| l.0 as isize == lparam.0)
            && !checked(hwnd, ID_SHOT_USE_DIR)
        {
            return crate::dark::dark_ctlcolor_dim(wparam);
        }
        // The hotkey-service status word: green when running/started, red otherwise.
        if msg == windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
            && GetDlgItem(Some(hwnd), ID_SHOT_STATUS).is_ok_and(|s| s.0 as isize == lparam.0)
        {
            let hdc = HDC(wparam.0 as *mut c_void);
            let running = GetDlgItem(Some(hwnd), ID_SHOT_STATUS)
                .map(|s| {
                    let txt = String::from_utf16_lossy(&control_text(s));
                    txt.contains("Running") || txt.contains("Started")
                })
                .unwrap_or(false);
            let col = if running { COLORREF(0x0059_C734) } else { COLORREF(0x004D_48E5) }; // green / red
            SetTextColor(hdc, col);
            windows::Win32::Graphics::Gdi::SetBkColor(hdc, DARK_BG());
            SetBkMode(hdc, TRANSPARENT);
            return LRESULT(dark_bg_brush().0 as isize);
        }
        // The Settings-sync status line: green when it reads "● Synced" (signed in), else a
        // muted grey (the signed-out invite / a transient "Connecting…"). Mirrors the
        // hotkey-service badge above.
        if msg == windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
            && GetDlgItem(Some(hwnd), ID_SYNC_STATUS).is_ok_and(|s| s.0 as isize == lparam.0)
        {
            let synced = GetDlgItem(Some(hwnd), ID_SYNC_STATUS)
                .map(|s| String::from_utf16_lossy(&control_text(s)).contains("Synced"))
                .unwrap_or(false);
            if synced {
                let hdc = HDC(wparam.0 as *mut c_void);
                SetTextColor(hdc, COLORREF(0x0059_C734)); // green
                windows::Win32::Graphics::Gdi::SetBkColor(hdc, DARK_BG());
                SetBkMode(hdc, TRANSPARENT);
                return LRESULT(dark_bg_brush().0 as isize);
            }
            return crate::dark::dark_ctlcolor_dim(wparam);
        }
        if let Some(r) = dark_ctlcolor(msg, wparam) {
            return r;
        }
        match msg {
            WM_CREATE => {
                // Bring up GDI+ for this window's lifetime so the dark-mode owner-draw can
                // render its toggle switches / icons / rounded buttons anti-aliased.
                GDIP_TOKEN.with(|t| t.set(crate::gdip::startup()));
                let hinst: HINSTANCE = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap().into();
                build_controls(hwnd, hinst);
                // Keep the hotkey-service status line live (so a self-heal on open, or a
                // watchdog restart, flips "Stopped" → "Running" without reopening).
                let _ = SetTimer(Some(hwnd), TIMER_SHOT_STATUS, 1000, None);
                // Lazy, throttled, background update check: it never blocks this window
                // opening, hits GitHub at most once a day (cached on disk in between), and
                // stays silent unless a newer release exists — then it posts WM_APP_UPDATE
                // to quietly nudge (no popup). See `update::lazy_check`.
                let target = hwnd.0 as isize;
                crate::update::lazy_check(move |tag| {
                    let raw = Box::into_raw(Box::new(tag));
                    let posted = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                        Some(HWND(target as *mut core::ffi::c_void)),
                        crate::update::WM_APP_UPDATE,
                        WPARAM(0),
                        LPARAM(raw as isize),
                    );
                    if posted.is_err() {
                        // The window vanished before delivery — reclaim the boxed tag.
                        drop(Box::from_raw(raw));
                    }
                });
                // If already signed in for settings sync, pull the cloud copy in the
                // background (applies to HKCU; takes effect for new thumbnails). No-op and
                // zero network when signed out.
                spawn_sync_pull(hwnd);
                LRESULT(0)
            }
            crate::update::WM_APP_UPDATE => {
                // A lazy background check found a newer release. Reclaim the boxed tag and
                // NON-intrusively relabel the "Check for updates" button into a quiet nudge
                // (no popup); clicking it still opens the About box, whose status pill shows the
                // update and offers the one-click install.
                let tag = if lparam.0 != 0 {
                    *Box::from_raw(lparam.0 as *mut String)
                } else {
                    String::new()
                };
                if let Ok(btn) = GetDlgItem(Some(hwnd), ID_CHECK_UPDATES) {
                    let label = if tag.is_empty() {
                        wide("Update available")
                    } else {
                        wide(&format!("Update to v{tag}"))
                    };
                    let _ = SetWindowTextW(btn, PCWSTR(label.as_ptr()));
                }
                LRESULT(0)
            }
            WM_APP_SYNC => {
                // A background sync op (sign-in / pull / disconnect) finished on a worker
                // thread. Reclaim the boxed event and update the UI on this message thread.
                if lparam.0 != 0 {
                    let event = *Box::from_raw(lparam.0 as *mut SyncEvent);
                    handle_sync_event(hwnd, event);
                }
                LRESULT(0)
            }
            WM_GETMINMAXINFO => {
                // Lock the WIDTH (vertical resize only) + a minimum height = the design
                // size. (No-op until the first WM_SIZE captures the design dimensions.)
                if let Some((w, h0)) = RESIZE.with(|s| s.borrow().as_ref().map(|st| (st.win_w, st.win_h0))) {
                    let mmi = &mut *(lparam.0 as *mut MINMAXINFO);
                    mmi.ptMinTrackSize.x = w;
                    mmi.ptMaxTrackSize.x = w;
                    mmi.ptMinTrackSize.y = h0;
                }
                LRESULT(0)
            }
            WM_SIZE => {
                let client_h = ((lparam.0 >> 16) & 0xFFFF) as i32;
                on_resize(hwnd, client_h);
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                let notify = ((wparam.0 >> 16) & 0xFFFF) as u32;
                match id {
                    IDOK => {
                        apply_settings(hwnd); // Save = apply only, keep the window open
                        spawn_sync_push(hwnd); // if signed in, mirror the change to the cloud
                    }
                    IDCANCEL => {
                        let _ = DestroyWindow(hwnd);
                    }
                    ID_SELECT_ALL | ID_CLEAR_ALL => {
                        // Affects the currently-shown (filtered) rows; the model
                        // syncs via LVN_ITEMCHANGED, so off-screen formats are kept.
                        if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
                            let on = id == ID_SELECT_ALL;
                            let count = SendMessageW(list, LVM_GETITEMCOUNT, None, None).0 as i32;
                            for i in 0..count {
                                set_check(list, i, on);
                            }
                        }
                    }
                    ID_SEARCH if notify == EN_CHANGE => {
                        if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
                            let text = get_edit_text(hwnd, ID_SEARCH);
                            // EN_CHANGE fires on every keystroke, and populate_list
                            // deletes + reinserts all FORMATS rows. Skip that whole rebuild
                            // when the NORMALIZED filter hasn't actually changed (a no-op
                            // edit, case-only change, or trailing whitespace).
                            let needle = text.trim().to_lowercase();
                            let changed = LAST_FILTER.with(|f| {
                                let mut f = f.borrow_mut();
                                if f.as_deref() == Some(needle.as_str()) {
                                    false
                                } else {
                                    *f = Some(needle);
                                    true
                                }
                            });
                            if changed {
                                populate_list(list, &text);
                            }
                        }
                    }
                    ID_DEFAULTS => reset_formats(hwnd), // file-type list only (see its tip)
                    ID_RESET_ALL => load_defaults(hwnd), // whole dialog → factory defaults
                    ID_MENU_RESET => {
                        if let Ok(mlist) = GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST) {
                            list::reset_menu_order(mlist);
                        }
                    }
                    // Instant-screenshot checkbox: enable/disable its hotkey picker live.
                    ID_SHOT_ENABLE => refresh_shot_status(hwnd),
                    ID_SHOT_QUICK_ENABLE => update_quick_enabled(hwnd),
                    ID_CUSTOM_ACTION_ENABLE => update_custom_action_enabled(hwnd),
                    ID_SHOT_USE_DIR => update_save_dir_enabled(hwnd),
                    ID_SYNC_BTN => on_sync_click(hwnd),
                    ID_SHOT_SET_DIR => {
                        // Pick the Ctrl+S save folder; persist immediately + refresh the
                        // display. (The toggle next to it is saved with the other settings
                        // on the Save button.)
                        if let Some(dir) = crate::win::pick_folder(hwnd) {
                            let _ = settings::set_screenshot_save_dir(&dir);
                            set_shot_dir_label(hwnd);
                        }
                    }
                    ID_SHOT_RESTART => {
                        // (Re)start the tray daemon: ensure the autostart entry + a
                        // live daemon, then re-register the current hotkey. Tick the
                        // Enable box to match, and show an optimistic status (the
                        // daemon was just spawned; the true state shows on reopen).
                        crate::screenshot::set_enabled(true);
                        crate::screenshot::reload_hotkey();
                        check(hwnd, ID_SHOT_ENABLE, true);
                        set_shot_status(hwnd, "Started");
                    }
                    ID_LANG if notify == CBN_SELCHANGE => on_lang_change(hwnd),
                    ID_ABOUT => show_about(hwnd),
                    ID_OPEN_LOG => open_diagnostics_log(),
                    ID_EDIT_UPLOAD_HOSTS => crate::screenshot::open_hosts_config(),
                    ID_EXPORT => export_settings_to_file(hwnd),
                    ID_IMPORT => import_settings_from_file(hwnd),
                    ID_REBUILD_CACHE => rebuild_thumbnail_cache(hwnd),
                    ID_REPAIR_ASSOC => repair_associations(hwnd),
                    ID_CHECK_UPDATES => show_about(hwnd),
                    nav if (ID_NAV_BASE..ID_NAV_BASE + NCAT as i32).contains(&nav) && notify == STN_CLICKED => {
                        switch_category(hwnd, (nav - ID_NAV_BASE) as usize);
                    }
                    ID_BANNER if notify == STN_CLICKED => {
                        // Open the currently-shown sponsor's link (or the product page
                        // if no sponsor feed loaded).
                        let mut url = None;
                        if let Some((_, rot)) = banner_rotator(hwnd) {
                            let r = &*rot;
                            if let Some(sponsor) = r.sponsors.get(r.cur) {
                                url = Some(wstr_to_string(&sponsor.link));
                            }
                        }
                        match url {
                            Some(u) if !u.is_empty() => open_url(&u),
                            _ => open_url(URL_PRODUCT),
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            // A footer SysLink or the banner tooltip is asking for its rotating text.
            WM_NOTIFY => {
                let nmhdr = lparam.0 as *const NMHDR;
                let code = (*nmhdr).code;
                // Drag-to-reorder the "Menu items" checklist: begin on LVN_BEGINDRAG.
                if code == windows::Win32::UI::Controls::LVN_BEGINDRAG
                    && (*nmhdr).hwndFrom == GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST).unwrap_or_default()
                {
                    let nmlv = lparam.0 as *const NMLISTVIEW;
                    list::begin_menu_drag((*nmhdr).hwndFrom, (*nmlv).iItem);
                    return LRESULT(0);
                }
                // Dark-mode modern restyle: own the paint of the format list, the
                // push buttons and the checkboxes via NM_CUSTOMDRAW. Light mode
                // returns nothing here, so the native themed look is unchanged.
                if code == NM_CUSTOMDRAW {
                    let from = (*nmhdr).hwndFrom;
                    if from == GetDlgItem(Some(hwnd), ID_LIST).unwrap_or_default() {
                        return LRESULT(restyle::draw_list_item(lparam.0 as *mut NMLVCUSTOMDRAW));
                    }
                    if is_button_class(from) {
                        return LRESULT(restyle::draw_button_cd(hwnd, lparam.0 as *const NMCUSTOMDRAW));
                    }
                    // SysLink credit etc. — let it draw itself.
                    return LRESULT(CDRF_DODEFAULT as isize);
                }
                // A FORMAT row's checkbox toggled → sync the model (FMT_STATE). Gate on
                // the source being the format list: the Menu-items checklist is also a
                // checkbox ListView and must NOT feed FMT_STATE (its state is read
                // directly in apply_settings).
                if code == LVN_ITEMCHANGED
                    && (*nmhdr).hwndFrom == GetDlgItem(Some(hwnd), ID_LIST).unwrap_or_default()
                    && !POPULATING.with(|p| p.get())
                {
                    let nmlv = lparam.0 as *const NMLISTVIEW;
                    if ((*nmlv).uChanged.0 & LVIF_STATE.0) != 0 {
                        let oldc = (*nmlv).uOldState & 0x3000;
                        let newc = (*nmlv).uNewState & 0x3000;
                        if oldc != newc {
                            let idx = (*nmlv).lParam.0 as usize;
                            let on = newc == CHECKED;
                            FMT_STATE.with(|s| {
                                if let Some(v) = s.borrow_mut().get_mut(idx) {
                                    *v = on;
                                }
                            });
                        }
                    }
                    return LRESULT(0);
                }
                if code == NM_CLICK || code == NM_RETURN {
                    let link = lparam.0 as *const NMLINK;
                    let url = wstr_to_string(&(*link).item.szUrl);
                    if !url.is_empty() {
                        open_url(&url);
                    }
                } else if code == TTN_GETDISPINFOW {
                    // Banner hover: hand back the current sponsor's tooltip. The buffer
                    // lives in the SponsorRotator (stable until WM_DESTROY frees it).
                    if let Some((banner, rot)) = banner_rotator(hwnd) {
                        if (*nmhdr).idFrom == banner.0 as usize {
                            let r = &*rot;
                            if let Some(sponsor) = r.sponsors.get(r.cur) {
                                let di = lparam.0 as *mut NMTTDISPINFOW;
                                (*di).lpszText = PWSTR(sponsor.tip.as_ptr() as *mut u16);
                            }
                        }
                    }
                }
                LRESULT(0)
            }
            // Right-click / Shift+F10 on the format list → bulk check/uncheck menu.
            WM_CONTEXTMENU if HWND(wparam.0 as *mut c_void) == GetDlgItem(Some(hwnd), ID_LIST).unwrap_or_default() => {
                list::list_context_menu(HWND(wparam.0 as *mut c_void), hwnd, lparam);
                LRESULT(0)
            }
            // Owner-drawn dark context-menu items (light text on dark).
            WM_MEASUREITEM => {
                let m = &mut *(lparam.0 as *mut MEASUREITEMSTRUCT);
                if m.CtlType == ODT_MENU {
                    let label = wide(list::ctx_menu_label(m.itemID as usize));
                    let n = label.len().saturating_sub(1);
                    let hdc = GetDC(Some(hwnd));
                    let old = SelectObject(hdc, HGDIOBJ(gui_font().0));
                    let mut sz = SIZE::default();
                    let _ = GetTextExtentPoint32W(hdc, &label[..n], &mut sz);
                    SelectObject(hdc, old);
                    ReleaseDC(Some(hwnd), hdc);
                    m.itemWidth = (sz.cx + 30) as u32;
                    m.itemHeight = 26;
                    LRESULT(1)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            WM_DRAWITEM => {
                let d = &*(lparam.0 as *const DRAWITEMSTRUCT);
                if d.CtlType == ODT_MENU {
                    let selected = (d.itemState.0 & ODS_SELECTED.0) != 0;
                    let bg = if selected { dark_menu_sel_brush() } else { dark_menu_brush() };
                    FillRect(d.hDC, &d.rcItem, bg);
                    SetBkMode(d.hDC, TRANSPARENT);
                    SetTextColor(d.hDC, DARK_TEXT());
                    SelectObject(d.hDC, HGDIOBJ(gui_font().0));
                    let mut label = wide(list::ctx_menu_label(d.itemID as usize));
                    let n = label.len().saturating_sub(1);
                    let mut rc = d.rcItem;
                    rc.left += 14;
                    DrawTextW(d.hDC, &mut label[..n], &mut rc, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
                    LRESULT(1)
                } else if d.CtlType == ODT_STATIC {
                    let cid = d.CtlID as i32;
                    if d.CtlID == ID_LEFT_MASK as u32 {
                        scroll::draw_left_mask(hwnd, d);
                    } else if cid == ID_PANE_HEADER {
                        draw_pane_header(hwnd, d);
                    } else if (ID_NAV_BASE..ID_NAV_BASE + NCAT as i32).contains(&cid) {
                        let active = NAV.with(|n| n.borrow().active) == (cid - ID_NAV_BASE) as usize;
                        draw_nav_item(hwnd, d, active);
                    } else {
                        // The owner-drawn section headers (uppercase label + divider).
                        restyle::draw_section_header(hwnd, d);
                    }
                    LRESULT(1)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            // Hand cursor over the clickable banner (so it reads as clickable).
            WM_SETCURSOR if HWND(wparam.0 as *mut c_void) == GetDlgItem(Some(hwnd), ID_BANNER).unwrap_or_default() => {
                let _ = SetCursor(LoadCursorW(None, IDC_HAND).ok());
                LRESULT(1)
            }
            // The sponsor feed arrived from the download thread: take ownership, show
            // the first sponsor (replacing the placeholder), and start the timers.
            WM_APP_SPONSORS => {
                if let Ok(banner) = GetDlgItem(Some(hwnd), ID_BANNER) {
                    let rot = lparam.0 as *mut SponsorRotator;
                    if !rot.is_null() {
                        // Swap in the new feed, freeing any prior one.
                        let prev = GetWindowLongPtrW(banner, GWLP_USERDATA) as *mut SponsorRotator;
                        let _ = KillTimer(Some(hwnd), TIMER_ROTATE);
                        SetWindowLongPtrW(banner, GWLP_USERDATA, rot as isize);
                        let r = &*rot;
                        // Free the bitmap currently in the static ONLY on the first
                        // swap (prev null = it still holds the embedded placeholder).
                        // A later feed's frames are rotator-owned and freed by
                        // drop_sponsor_rotator below, so freeing them here too would
                        // double-free that GDI object.
                        show_current_image(hwnd, banner, r, prev.is_null());
                        if r.rotates() {
                            let _ = SetTimer(Some(hwnd), TIMER_ROTATE, r.rotate_ms, None);
                        }
                        if !prev.is_null() {
                            // The banner tooltip pulls its text by pointer from the
                            // shown sponsor (callback-driven). If a hint for the *prev*
                            // feed is on screen, dismiss it (TTM_POP) before freeing
                            // that feed — otherwise it would point at freed memory.
                            let tip = HWND(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut c_void);
                            if !tip.is_invalid() {
                                SendMessageW(tip, TTM_POP, None, None);
                            }
                            drop_sponsor_rotator(prev);
                        }
                    }
                } else {
                    drop_sponsor_rotator(lparam.0 as *mut SponsorRotator); // window gone
                }
                LRESULT(0)
            }
            // Keep the hotkey-service status line honest while the dialog is open.
            WM_TIMER if wparam.0 == TIMER_SHOT_STATUS => {
                refresh_shot_status(hwnd);
                LRESULT(0)
            }
            // Advance the current image's GIF animation one frame (frames are reused
            // each loop, so don't free the prior one; WM_DESTROY frees them all).
            WM_TIMER if wparam.0 == TIMER_BANNER => {
                if let Some((banner, rot)) = banner_rotator(hwnd) {
                    let r = &mut *rot;
                    let (cur, imgi) = (r.cur, r.img);
                    let nframes = r.sponsors.get(cur).and_then(|a| a.images.get(imgi)).map_or(0, |im| im.frames.len());
                    if nframes > 1 {
                        r.frame = (r.frame + 1) % nframes;
                        let f = r.sponsors[cur].images[imgi].frames[r.frame];
                        SendMessageW(banner, STM_SETIMAGE, Some(WPARAM(IMAGE_BITMAP.0 as usize)), Some(LPARAM(f)));
                    }
                }
                LRESULT(0)
            }
            // Rotate to the next sponsor / image: advance the rotator, then show the
            // new art (raw STM_SETIMAGE so the prior bitmap survives — the rotator
            // still owns it). The tooltip pulls the fresh text on the next hover.
            WM_TIMER if wparam.0 == TIMER_ROTATE => {
                if let Some((banner, rot)) = banner_rotator(hwnd) {
                    (*rot).advance();
                    show_current_image(hwnd, banner, &*rot, false);
                }
                LRESULT(0)
            }
            // All background painting is owned by WM_PAINT (double-buffered below), so
            // suppress the default erase: returning 1 stops DefWindowProcW from filling
            // the invalid band with the class brush as a SEPARATE deferred frame — that
            // erase-then-paint two-step is the white/gray flash on a fast left scroll.
            WM_ERASEBKGND => LRESULT(1),
            // Paint the dialog background + the "chrome" (rounded list card / input +
            // dropdown field frames behind their controls / hairline dividers) into an
            // off-screen buffer, then blit once — so the fill and the chrome land in the
            // SAME frame instead of flashing the bare background between them. The blit is
            // clipped to non-child pixels by WS_CLIPCHILDREN, so the child controls keep
            // their own (SetWindowPos-preserved) pixels and aren't briefly overpainted.
            // The fill brush MIRRORS the class hbrBackground (main.rs) exactly so light
            // mode is byte-identical to before (COLOR_BTNFACE, not the 243 surface tone).
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let pr = ps.rcPaint;
                let (pw, ph) = (pr.right - pr.left, pr.bottom - pr.top);
                if pw > 0 && ph > 0 {
                    let mem = CreateCompatibleDC(Some(hdc));
                    let bmp = CreateCompatibleBitmap(hdc, pw, ph);
                    let old = SelectObject(mem, HGDIOBJ(bmp.0));
                    // Map client coords onto the dirty-rect-sized buffer so paint_chrome
                    // (which works in client coords) draws into the right place.
                    let _ = SetViewportOrgEx(mem, -pr.left, -pr.top, None);
                    let br = if is_dark() { dark_bg_brush() } else { HBRUSH(16isize as *mut c_void) };
                    FillRect(mem, &pr, br);
                    restyle::paint_chrome(hwnd, mem);
                    let _ = SetViewportOrgEx(mem, 0, 0, None);
                    let _ = BitBlt(hdc, pr.left, pr.top, pw, ph, Some(mem), 0, 0, SRCCOPY);
                    SelectObject(mem, old);
                    let _ = DeleteObject(HGDIOBJ(bmp.0));
                    let _ = DeleteDC(mem);
                }
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            // Left-column scrolling (dark mode): the scrollbar + the mouse wheel.
            WM_VSCROLL => {
                scroll::on_vscroll(hwnd, wparam, lparam);
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                let wheel = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let pos = scroll::SCROLL.with(|s| s.borrow().pos);
                scroll::scroll_to(hwnd, pos - wheel / 120 * dpi_scale(hwnd, 42));
                LRESULT(0)
            }
            WM_DPICHANGED => {
                wm_dpichanged(hwnd, lparam);
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = KillTimer(Some(hwnd), TIMER_SHOT_STATUS);
                // Stop + free the sponsor rotation (both timers + every sponsor's bitmaps).
                if let Ok(banner) = GetDlgItem(Some(hwnd), ID_BANNER) {
                    let _ = KillTimer(Some(hwnd), TIMER_BANNER);
                    let _ = KillTimer(Some(hwnd), TIMER_ROTATE);
                    let rot = GetWindowLongPtrW(banner, GWLP_USERDATA) as *mut SponsorRotator;
                    if !rot.is_null() {
                        SetWindowLongPtrW(banner, GWLP_USERDATA, 0);
                        drop_sponsor_rotator(rot);
                    } else {
                        // No sponsor feed ever installed (the gate passed — the manifest
                        // listed sponsors — but every image download/decode failed, so
                        // WM_APP_SPONSORS never posted a rotator). The banner still holds
                        // the embedded placeholder set in build_controls; a STATIC does
                        // NOT free a STM_SETIMAGE bitmap, so reclaim it here or it leaks
                        // one GDI bitmap per opened Settings window.
                        let prev = SendMessageW(
                            banner,
                            STM_SETIMAGE,
                            Some(WPARAM(IMAGE_BITMAP.0 as usize)),
                            Some(LPARAM(0)),
                        );
                        if prev.0 != 0 {
                            let _ = DeleteObject(HGDIOBJ(prev.0 as *mut c_void));
                        }
                    }
                }
                scroll::SCROLL.with(|s| *s.borrow_mut() = scroll::ScrollData::default());
                GDIP_TOKEN.with(|t| {
                    let tok = t.replace(0);
                    if tok != 0 {
                        crate::gdip::shutdown(tok);
                    }
                });
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

