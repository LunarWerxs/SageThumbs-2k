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
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
    DrawTextW, EndPaint, FillRect, GetDC, GetStockObject, GetTextExtentPoint32W, InvalidateRect,
    RedrawWindow, ReleaseDC, ScreenToClient, SelectObject, SetBkMode, SetDCBrushColor,
    SetTextCharacterExtra, SetTextColor, SetViewportOrgEx, DC_BRUSH, DT_CENTER, DT_END_ELLIPSIS,
    DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HBRUSH, HDC, HGDIOBJ, PAINTSTRUCT,
    RDW_ALLCHILDREN, RDW_INVALIDATE, RDW_UPDATENOW, SRCCOPY, TRANSPARENT,
};
use windows::Win32::UI::Controls::{
    SetScrollInfo, CDDS_ITEMPOSTPAINT, CDDS_ITEMPREPAINT, CDDS_PREPAINT, CDDS_SUBITEM, CDIS_FOCUS,
    CDIS_HOT, CDIS_SELECTED, CDRF_DODEFAULT, CDRF_NEWFONT, CDRF_NOTIFYITEMDRAW,
    CDRF_NOTIFYPOSTPAINT, CDRF_NOTIFYSUBITEMDRAW, CDRF_SKIPDEFAULT, DRAWITEMSTRUCT,
    LIST_VIEW_ITEM_STATE_FLAGS, LVCFMT_LEFT, LVCF_FMT, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW,
    LVIF_PARAM, LVIF_STATE, LVIF_TEXT, LVIS_STATEIMAGEMASK, LVITEMW, LVM_DELETEALLITEMS,
    LVM_GETHEADER, LVM_GETITEMCOUNT, LVM_GETITEMRECT, LVM_GETITEMSTATE, LVM_GETNEXTITEM,
    LVM_GETSELECTEDCOUNT, LVM_INSERTCOLUMNW, LVM_INSERTITEMW, LVM_SETBKCOLOR, LVM_SETCOLUMNW,
    LVM_SETCOLUMNWIDTH, LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMSTATE, LVM_SETITEMW,
    LVM_SETTEXTBKCOLOR, LVM_SETTEXTCOLOR, LVNI_FOCUSED, LVNI_SELECTED, LVN_ITEMCHANGED,
    LVS_EX_CHECKBOXES, LVS_EX_FULLROWSELECT, LVS_NOCOLUMNHEADER, LVS_NOSORTHEADER, LVS_REPORT,
    MEASUREITEMSTRUCT, NMCUSTOMDRAW, NMHDR, NMLINK, NMLISTVIEW, NMLVCUSTOMDRAW, NMTTDISPINFOW,
    NM_CLICK, NM_CUSTOMDRAW, NM_RETURN, ODT_MENU, ODT_STATIC, TTF_IDISHWND, TTF_SUBCLASS,
    TTM_ADDTOOLW, TTM_POP, TTM_SETMAXTIPWIDTH, TTN_GETDISPINFOW, TTTOOLINFOW, WC_LISTVIEWW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_SPACE;
use windows::Win32::UI::Shell::{
    DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use sagethumbs2k_core::{default_menu_tokens, formats, i18n, settings, MENU_SEP_TOKEN};

use crate::about::show_about;
use crate::dark::{
    dark_bg_brush, dark_control, dark_ctlcolor, dark_theme_combo, is_dark, ACCENT, ACCENT_HOT,
    ACCENT_PRESS, ACCENT_TEXT, BORDER, BORDER_STRONG, BTN_FACE, BTN_FACE_HOT, BTN_FACE_PRESS,
    CHECK_BG, DARK_BG, DARK_TEXT, DISABLED_TEXT, HEADER_TEXT, INPUT_BG, ON_ACCENT, SEL_BG, SURFACE,
    ZEBRA,
};
use crate::sponsors::{
    drop_sponsor_rotator, show_current_image, sponsors_enabled, SponsorRotator, TIMER_BANNER,
    TIMER_ROTATE, WM_APP_SPONSORS,
};
use crate::win::{
    check, checked, ctl, dpi_scale, get_edit_text, gui_font_for, gui_font_header, message_box,
    open_url, t, wide, wm_dpichanged, wstr_to_string, BTN_H, BUTTON, CHECKED, COMBOBOX, EDIT,
    EDIT_X, IDCANCEL, IDOK, INDENT, LABEL_W, MARGIN, SS_BITMAP, SS_NOTIFY, SS_OWNERDRAW,
    SS_REALSIZECONTROL, STATIC, SYSLINK, TTS_ALWAYSTIP, TTS_NOPREFIX, UNCHECKED, URL_PARENT,
    URL_PRODUCT,
};

// Submodules split out of this (formerly ~2030-line) file. They're descendants of
// this module, so they freely call its private helpers via `super::` (s, fill,
// control_text, set_check, is_checked, …); the parent reaches their entry points
// via the module path (restyle::…, scroll::…, list::…).
mod list;
mod restyle; // dark-mode owner-draw painting + the combo/scrollbar subclasses
mod scroll; // the left-column scroll subsystem (incl. its clipping mask) // the self-contained ListView subclass + bulk-toggle context menu

mod ids;
pub(super) use ids::*;
mod build;
use build::*;
/// The Business-licence reminder strip: its pixels, its click, and where it sits. Sibling of
/// `nudge` below (same strip mechanism), but for `license::Posture::BusinessNag` /
/// `DeauthorizedLoud` instead of the sign-in campaign — see that module's doc comment.
mod biznag;
mod helpers;
/// The Licence Settings page: seeding its two status lines, the Redeem/Check-now worker
/// calls, and `WM_APP_LICENCE`'s completion handling. Modelled on `sync`'s
/// `WM_APP_SYNC` pattern below.
mod licence_ui;
mod localize;
mod menuitems;
mod navrail;
/// The "you could be signed in" banner: its pixels, its clicks, and where it sits.
mod nudge;

/// Ask the sign-in engine whether to show its banner in the window about to be built.
///
/// Must be called BEFORE the window is created, because the answer changes how tall it is: the
/// banner lives in a strip between the pane and the footer, and the page layout below it runs
/// exactly once. Returns whether a banner will be shown.
pub(crate) fn decide_sign_in_nudge() -> bool {
    nudge::decide()
}

/// Design-pixel height the banner strip adds to the window. Pair with [`decide_sign_in_nudge`].
pub(crate) fn sign_in_nudge_height() -> i32 {
    nudge::strip_h()
}

/// Ask the licence engine whether the Business-nag strip will show, the same way
/// [`decide_sign_in_nudge`] asks the sign-in one — before the window is created, because the
/// answer changes how tall it is.
pub(crate) fn decide_business_nag() -> bool {
    biznag::decide()
}

/// Design-pixel height the Business-nag strip adds to the window. Pair with
/// [`decide_business_nag`].
pub(crate) fn business_nag_height() -> i32 {
    biznag::strip_h()
}

mod daemon_status;
mod licence_state;
mod menu_rows;
mod resize;
mod search;
mod shot;
mod sync;
mod tooltips;
mod values;
use daemon_status::*;
use helpers::*;
use licence_state::*;
pub(crate) use licence_state::{
    format_unix_date, licence_page, licence_reminder_body, licence_state_line,
};
use licence_ui::*;
use localize::*;
use menu_rows::*;
use navrail::*;
use resize::*;
pub(crate) use shot::{run_shot, run_shot_gif, run_shot_search};
use sync::*;
use tooltips::*;
use values::*;
mod commands;
use commands::*;
mod notify;
use notify::*;
mod paintmsg;
use paintmsg::*;
mod lifecycle;
use lifecycle::*;
// Win32 message consts the `windows` crate omits (local so they shadow the `WindowsAndMessaging::*` glob).
const EM_SETCUEBANNER: u32 = 0x1501;
const CB_SETDROPPEDWIDTH: u32 = 0x0160;

#[derive(Clone, Copy)]
pub(super) struct SponsorLayout {
    foot_y: i32,
    credit_y: i32,
}

/// Initial creation-time position for the footer row (About/Close/Save) and the
/// credit line. `apply_v3_layout` (`navrail.rs`) unconditionally repositions all
/// three afterward, so this only matters for the brief window between control
/// creation and that reflow — always the no-banner spacing since ID_BANNER is
/// permanently hidden in the v3 shell (see build.rs's sponsor-promotion comment;
/// A093/A264, 2026-08-15).
pub(super) fn sponsor_layout(_dark: bool) -> SponsorLayout {
    let foot_y = 470;
    SponsorLayout {
        foot_y,
        credit_y: foot_y + 6,
    }
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
        ctl(
            self.hwnd, STATIC, text, style, MARGIN, self.y, 322, 18, id, self.hinst,
        );
        self.y += 18;
    }

    /// A full-width checkbox row. Kept compact (20px tall, small lead gap) so the
    /// stack of left-column options stays short.
    unsafe fn checkbox(&mut self, text: &str, style: WINDOW_STYLE, w: i32, id: i32) {
        self.y += MT_CHECK;
        ctl(
            self.hwnd, BUTTON, text, style, INDENT, self.y, w, 20, id, self.hinst,
        );
        self.y += 20;
    }

    /// `label:` + a right-aligned numeric edit; returns the edit hwnd. The label is
    /// dropped 1px to sit against the field. `lbl_id` keeps it live-retranslatable AND
    /// tooltip-targetable, and `SS_NOTIFY` makes it mouse-receptive so a hover tooltip on
    /// the label fires (a plain static is click-through, so its hint never showed).
    unsafe fn edit(&mut self, label: &str, lbl_id: i32, style: WINDOW_STYLE, id: i32) -> HWND {
        self.y += MT_FIELD;
        ctl(
            self.hwnd,
            STATIC,
            label,
            WINDOW_STYLE(SS_NOTIFY),
            INDENT,
            self.y + 1,
            LABEL_W,
            18,
            lbl_id,
            self.hinst,
        );
        let e = ctl(
            self.hwnd, EDIT, "", style, EDIT_X, self.y, 84, 18, id, self.hinst,
        );
        self.y += 18;
        e
    }

    /// `label:` + a dropdown combo at x=160; returns the combo hwnd for the caller to
    /// fill + theme. `lbl_id` keeps the label live-retranslatable.
    unsafe fn combo(&mut self, label: &str, lbl_id: i32, drop_h: i32, id: i32) -> HWND {
        self.y += MT_FIELD;
        ctl(
            self.hwnd,
            STATIC,
            label,
            WINDOW_STYLE(0),
            INDENT,
            self.y + 4,
            130,
            18,
            lbl_id,
            self.hinst,
        );
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
        let list = ctl(
            self.hwnd,
            WC_LISTVIEWW,
            "",
            style,
            MARGIN,
            self.y,
            322,
            h,
            id,
            self.hinst,
        );
        // Theme the list surface (SURFACE()/DARK_TEXT() are theme-aware). NOT
        // applying DarkMode_Explorer in either theme — it gives dark check glyphs +
        // a scrollbar that vanishes on the surface.
        theme_checkbox_list(list);
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
        ctl(
            self.hwnd, BUTTON, text, WS_TABSTOP, INDENT, self.y, w, 24, id, self.hinst,
        );
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
                ctl(
                    self.hwnd, BUTTON, text, WS_TABSTOP, x, self.y, w, 24, id, self.hinst,
                );
            }
        }
        self.y += 24;
    }

    /// A single line of dynamic status text (e.g. the hotkey-service state), empty at
    /// build time and filled later via SetDlgItemText. Checkbox-tight gap above.
    unsafe fn status(&mut self, id: i32) {
        self.y += MT_CHECK;
        ctl(
            self.hwnd,
            STATIC,
            "",
            WINDOW_STYLE(0),
            INDENT,
            self.y + 2,
            300,
            18,
            id,
            self.hinst,
        );
        self.y += 18;
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
    SendMessageW(
        list,
        LVM_INSERTCOLUMNW,
        Some(WPARAM(idx as usize)),
        Some(LPARAM(&mut col as *mut _ as isize)),
    );
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
    SendMessageW(
        list,
        LVM_SETITEMW,
        Some(WPARAM(0)),
        Some(LPARAM(&sub as *const _ as isize)),
    );
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

/// Localized short label for the Settings format list's "How" column (audit E03):
/// the capability's source kind, with an " (OS codec)" suffix when the format's decode
/// route depends on one that may not be installed. `formats::capability` is the single
/// source of truth for the underlying fact; this is presentation only.
pub(super) fn capability_label(ext: &str) -> String {
    let cap = formats::capability(ext);
    let base = match cap.source {
        formats::Source::FullDecode => t("cap_full_decode"),
        formats::Source::EmbeddedPreview => t("cap_embedded_preview"),
        formats::Source::CoverArt => t("cap_cover_art"),
        formats::Source::CoverOrFirstPage => t("cap_cover_or_first_page"),
        formats::Source::VideoFrame => t("cap_video_frame"),
        formats::Source::ContainedImages => t("cap_contained_images"),
    };
    match cap.os_codec {
        Some(_) => format!("{base} ({})", t("cap_os_codec")),
        None => base.to_string(),
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
        SendMessageW(
            list,
            LVM_INSERTITEMW,
            Some(WPARAM(0)),
            Some(LPARAM(&mut item as *mut _ as isize)),
        );
        set_subitem(list, row, 1, cat);
        set_subitem(list, row, 2, &capability_label(ext));
        set_subitem(list, row, 3, desc);
        set_check(list, row, *state.get(i).unwrap_or(&false));
        row += 1;
    }
    fit_columns(list);
}

/// Size the Description column to fill the list's current visible width — no dead
/// gap, no horizontal scroll. Re-run after a filter (the scrollbar may toggle), and
/// after the user drags either of the two columns to its left.
///
/// It is now UNCONDITIONAL. There used to be a thread-local "the user dragged Description, so
/// leave it alone" flag guarding this, which existed only so the auto-fit would not snap such a
/// drag straight back. `list.rs` refuses that drag outright (dragging the last column can only
/// open dead space against the scrollbar), so the flag guarded a case that can no longer happen,
/// and it carried a real hazard of its own: being thread-local rather than per-window, a
/// second Settings window in the same process inherited it and could keep a permanent dead gap.
pub(super) unsafe fn fit_columns(list: HWND) {
    let mut crc = RECT::default();
    let _ = GetClientRect(list, &mut crc);
    // MEASURE the extension + category columns rather than assuming 64 + 92. Those were the
    // creation widths, and they were also a silent dependency: the moment the user could drag
    // them (issue #26.3) a hard-coded pair would leave Description overlapping or short by
    // exactly however far the drag went.
    let fixed: i32 = (0..3)
        .map(|c| {
            SendMessageW(
                list,
                windows::Win32::UI::Controls::LVM_GETCOLUMNWIDTH,
                Some(WPARAM(c)),
                None,
            )
            .0 as i32
        })
        .sum();
    let descw = ((crc.right - crc.left) - fixed).max(80);
    SendMessageW(
        list,
        LVM_SETCOLUMNWIDTH,
        Some(WPARAM(3)),
        Some(LPARAM(descw as isize)),
    );
}

/// How many Settings pages exist — the bound `--tab N` is validated against.
pub(crate) const NAV_CATEGORY_COUNT: usize = navrail::NCAT;
/// Control id of the FIRST nav-rail item; page `n`'s item is `NAV_ID_BASE + n`.
pub(crate) const NAV_ID_BASE: i32 = ID_NAV_BASE;

/// Show Settings page `ci`. The wrapper exists so `main`'s `--tab` can reach the nav rail
/// without `navrail`'s internals becoming crate-visible.
///
/// # Safety
/// `hwnd` must be a live Settings window whose controls have been built.
pub(crate) unsafe fn show_category(hwnd: HWND, ci: usize) {
    switch_category(hwnd, ci);
}

/// The nav-rail index of the **Quick preview** page, for `--tab`. Resolved by NAME through
/// `navrail::category_index` rather than written as a literal, so inserting a Settings page
/// cannot re-point the Quick preview viewer's caption gear at somebody else's page.
pub(crate) fn quick_preview_page() -> usize {
    navrail::category_index("nav_quickpreview").unwrap_or(0)
}
pub(crate) extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if let Some(r) = special_ctlcolor(hwnd, msg, wparam, lparam) {
            return r;
        }
        if let Some(r) = on_lifecycle_msg(hwnd, msg, wparam, lparam) {
            return r;
        }
        if let Some(r) = on_command_or_notify_msg(hwnd, msg, wparam, lparam) {
            return r;
        }
        if let Some(r) = on_paint_msg(hwnd, msg, wparam, lparam) {
            return r;
        }
        if let Some(r) = on_timer_or_scroll_msg(hwnd, msg, wparam, lparam) {
            return r;
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

#[cfg(test)]
mod tests;
