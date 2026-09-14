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
    NM_CLICK, NM_CUSTOMDRAW, NM_RETURN, ODS_SELECTED, ODT_MENU, ODT_STATIC, TTF_IDISHWND,
    TTF_SUBCLASS, TTM_ADDTOOLW, TTM_POP, TTM_SETMAXTIPWIDTH, TTN_GETDISPINFOW, TTTOOLINFOW,
    WC_LISTVIEWW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_SPACE;
use windows::Win32::UI::Shell::{
    DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use sagethumbs2k_core::{default_menu_tokens, formats, i18n, settings, MENU_SEP_TOKEN};

use crate::about::show_about;
use crate::dark::{
    dark_bg_brush, dark_control, dark_ctlcolor, dark_menu_brush, dark_menu_sel_brush,
    dark_theme_combo, is_dark, ACCENT, ACCENT_HOT, ACCENT_PRESS, ACCENT_TEXT, BORDER,
    BORDER_STRONG, BTN_FACE, BTN_FACE_HOT, BTN_FACE_PRESS, CHECK_BG, DARK_BG, DARK_TEXT,
    DISABLED_TEXT, HEADER_TEXT, INPUT_BG, ON_ACCENT, SEL_BG, SURFACE, ZEBRA,
};
use crate::sponsors::{
    drop_sponsor_rotator, show_current_image, sponsors_enabled, SponsorRotator, TIMER_BANNER,
    TIMER_ROTATE, WM_APP_SPONSORS,
};
use crate::win::{
    check, checked, ctl, dpi_scale, get_edit_text, gui_font, gui_font_for, gui_font_header,
    message_box, open_url, t, wide, wm_dpichanged, wstr_to_string, BTN_H, BUTTON, CHECKED,
    COMBOBOX, EDIT, EDIT_X, IDCANCEL, IDOK, INDENT, LABEL_W, MARGIN, SS_BITMAP, SS_NOTIFY,
    SS_OWNERDRAW, SS_REALSIZECONTROL, STATIC, SYSLINK, TTS_ALWAYSTIP, TTS_NOPREFIX, UNCHECKED,
    URL_PARENT, URL_PRODUCT,
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

/// The dialog's WM_CTLCOLORSTATIC overrides that key off LIVE control state (a
/// dependent checkbox, a running/synced status word) rather than just window class —
/// `dark_ctlcolor` handles the class-generic theming. Checked once, before the main
/// message dispatch; `None` means fall through to it. `Some` short-circuits the whole
/// wndproc, exactly like the pre-match block this replaces.
unsafe fn special_ctlcolor(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    // The Quick-save hotkey label stays ENABLED (a disabled static draws an
    // etched/blurry look in dark mode) but reads as greyed when instant
    // screenshot is off — paint its text dim here instead of the normal color.
    if msg == windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
        && GetDlgItem(Some(hwnd), ID_LBL_SHOT_QUICK_HK).is_ok_and(|l| l.0 as isize == lparam.0)
        && !checked(hwnd, ID_SHOT_QUICK_ENABLE)
    {
        return Some(crate::dark::dark_ctlcolor_dim(wparam));
    }
    // The save-folder display greys with the "Save to a set folder" toggle (same as
    // the quick-hotkey label — a disabled static draws etched in dark mode).
    if msg == windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
        && GetDlgItem(Some(hwnd), ID_SHOT_DIR).is_ok_and(|l| l.0 as isize == lparam.0)
        && !checked(hwnd, ID_SHOT_USE_DIR)
    {
        return Some(crate::dark::dark_ctlcolor_dim(wparam));
    }
    // The hotkey-service status word: green when running/started, red otherwise.
    if msg == windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
        && GetDlgItem(Some(hwnd), ID_SHOT_STATUS).is_ok_and(|s| s.0 as isize == lparam.0)
    {
        let hdc = HDC(wparam.0 as *mut c_void);
        // Decided from the typed state SHOT_STATUS_GREEN was set to alongside the text
        // (see set_shot_status), not by sniffing the (eventually localized) label for
        // English words, which broke silently in every non-English build.
        let running = SHOT_STATUS_GREEN.with(|g| g.get());
        let col = if running {
            COLORREF(0x0059_C734)
        } else {
            COLORREF(0x004D_48E5)
        }; // green / red
        SetTextColor(hdc, col);
        windows::Win32::Graphics::Gdi::SetBkColor(hdc, DARK_BG());
        SetBkMode(hdc, TRANSPARENT);
        return Some(LRESULT(dark_bg_brush().0 as isize));
    }
    // The Settings-sync status line: green in a healthy synced state, else a muted grey
    // (the signed-out invite / a transient "Connecting…"). Mirrors the hotkey-service
    // badge above. The state is asked for directly rather than sniffed out of the
    // control's text: that used to match the English word "Synced", which would have
    // gone grey in all 35 translations the moment the line was localized, with nothing
    // failing to say so.
    if msg == windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
        && GetDlgItem(Some(hwnd), ID_SYNC_STATUS).is_ok_and(|s| s.0 as isize == lparam.0)
    {
        if sync::sync_status_is_green() {
            let hdc = HDC(wparam.0 as *mut c_void);
            SetTextColor(hdc, COLORREF(0x0059_C734)); // green
            windows::Win32::Graphics::Gdi::SetBkColor(hdc, DARK_BG());
            SetBkMode(hdc, TRANSPARENT);
            return Some(LRESULT(dark_bg_brush().0 as isize));
        }
        return Some(crate::dark::dark_ctlcolor_dim(wparam));
    }
    // The licence-state line: green when actively licensed, red when revoked, the plain
    // theme colour otherwise (Personal / no key entered yet — a normal state, not a
    // problem one). Same green/red pair the hotkey-service and sync badges above use.
    if msg == windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
        && GetDlgItem(Some(hwnd), ID_LICENCE_STATE_STATUS).is_ok_and(|s| s.0 as isize == lparam.0)
    {
        return match licence_ui::state_tone() {
            // Not a special case: hand it to the generic class-based theming instead of
            // returning `None` here, which would skip dark theming for this control
            // entirely (this `if` has already committed to answering for it).
            licence_ui::Tone::Neutral => dark_ctlcolor(msg, wparam),
            licence_ui::Tone::Good => {
                let hdc = HDC(wparam.0 as *mut c_void);
                SetTextColor(hdc, COLORREF(0x0059_C734)); // green
                windows::Win32::Graphics::Gdi::SetBkColor(hdc, DARK_BG());
                SetBkMode(hdc, TRANSPARENT);
                Some(LRESULT(dark_bg_brush().0 as isize))
            }
            licence_ui::Tone::Bad => {
                let hdc = HDC(wparam.0 as *mut c_void);
                SetTextColor(hdc, COLORREF(0x004D_48E5)); // red
                windows::Win32::Graphics::Gdi::SetBkColor(hdc, DARK_BG());
                SetBkMode(hdc, TRANSPARENT);
                Some(LRESULT(dark_bg_brush().0 as isize))
            }
        };
    }
    // The redeem-result line, same tri-state (idle / just-redeemed / just-rejected).
    if msg == windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
        && GetDlgItem(Some(hwnd), ID_LICENCE_REDEEM_STATUS).is_ok_and(|s| s.0 as isize == lparam.0)
    {
        return match licence_ui::redeem_tone() {
            licence_ui::Tone::Neutral => dark_ctlcolor(msg, wparam),
            licence_ui::Tone::Good => {
                let hdc = HDC(wparam.0 as *mut c_void);
                SetTextColor(hdc, COLORREF(0x0059_C734)); // green
                windows::Win32::Graphics::Gdi::SetBkColor(hdc, DARK_BG());
                SetBkMode(hdc, TRANSPARENT);
                Some(LRESULT(dark_bg_brush().0 as isize))
            }
            licence_ui::Tone::Bad => {
                let hdc = HDC(wparam.0 as *mut c_void);
                SetTextColor(hdc, COLORREF(0x004D_48E5)); // red
                windows::Win32::Graphics::Gdi::SetBkColor(hdc, DARK_BG());
                SetBkMode(hdc, TRANSPARENT);
                Some(LRESULT(dark_bg_brush().0 as isize))
            }
        };
    }
    dark_ctlcolor(msg, wparam)
}

/// Window-lifecycle + app-posted messages: creation, teardown, resize limits, the
/// background update/sync callbacks, and the sponsor feed arriving. `None` means the
/// message isn't one of these — fall through to the next dispatch group.
unsafe fn on_lifecycle_msg(
    hwnd: HWND,
    msg: u32,
    _wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    match msg {
        WM_CREATE => Some(on_create(hwnd)),
        crate::update::WM_APP_UPDATE => Some(on_update_available(hwnd, lparam)),
        WM_APP_SYNC => Some(on_app_sync(hwnd, lparam)),
        WM_APP_CACHE => Some(on_app_cache(hwnd, lparam)),
        WM_APP_LICENCE => Some(on_app_licence(hwnd, lparam)),
        WM_GETMINMAXINFO => Some(on_getminmaxinfo(lparam)),
        WM_SIZE => {
            let client_h = ((lparam.0 >> 16) & 0xFFFF) as i32;
            on_resize(hwnd, client_h);
            Some(LRESULT(0))
        }
        WM_APP_SPONSORS => Some(on_app_sponsors(hwnd, lparam)),
        WM_DPICHANGED => {
            wm_dpichanged(hwnd, lparam);
            Some(LRESULT(0))
        }
        WM_CLOSE => {
            close_settings(hwnd);
            Some(LRESULT(0))
        }
        WM_DESTROY => Some(on_destroy(hwnd)),
        _ => None,
    }
}

/// The dialog's one exit path — WM_CLOSE (the window X / Alt+F4) and IDCANCEL (the "Close"
/// button) used to each carry their own copy of this. Blocks up to 6s flushing any pending
/// sync push before tearing the window down, so a Save right before closing isn't lost to a
/// race with the background push.
unsafe fn close_settings(hwnd: HWND) {
    crate::sync_client::flush_pending(std::time::Duration::from_secs(6));
    let _ = DestroyWindow(hwnd);
}

unsafe fn on_create(hwnd: HWND) -> LRESULT {
    // Bring up GDI+ for this window's lifetime so the dark-mode owner-draw can
    // render its toggle switches / icons / rounded buttons anti-aliased.
    GDIP_TOKEN.with(|t| t.set(crate::gdip::startup()));
    let hinst: HINSTANCE = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
        .unwrap()
        .into();
    build_controls(hwnd, hinst);
    // Keep the hotkey-service status line live (so a self-heal on open,
    // or a later stop, is reflected without reopening).
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

unsafe fn on_update_available(hwnd: HWND, lparam: LPARAM) -> LRESULT {
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

unsafe fn on_app_sync(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    // A background sync op (sign-in / pull / disconnect) finished on a worker
    // thread. Reclaim the boxed event and update the UI on this message thread.
    if lparam.0 != 0 {
        let event = *Box::from_raw(lparam.0 as *mut SyncEvent);
        handle_sync_event(hwnd, event);
    }
    LRESULT(0)
}

/// A background licence op (redeem / check-now) finished on a worker thread. Reclaim the
/// boxed event and update the Licence page on this message thread — same reclaim shape as
/// [`on_app_sync`] just above.
unsafe fn on_app_licence(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    if lparam.0 != 0 {
        let event = *Box::from_raw(lparam.0 as *mut LicenceEvent);
        handle_licence_event(hwnd, event);
    }
    LRESULT(0)
}

/// A background `spawn_cache_rebuild` worker finished (thumbnail cache clear + Explorer
/// restart). Reclaim the boxed event, re-enable the window, and show its follow-up message
/// (if any) now that the restart has actually completed.
unsafe fn on_app_cache(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
    if lparam.0 != 0 {
        let event = *Box::from_raw(lparam.0 as *mut CacheRebuiltEvent);
        let _ = EnableWindow(hwnd, true);
        if let Some((text, caption)) = event.after {
            msg(hwnd, text, caption, MB_ICONINFORMATION);
        }
    }
    LRESULT(0)
}

unsafe fn on_getminmaxinfo(lparam: LPARAM) -> LRESULT {
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

/// The sponsor feed arrived from the download thread: take ownership, show
/// the first sponsor (replacing the placeholder), and start the timers.
unsafe fn on_app_sponsors(hwnd: HWND, lparam: LPARAM) -> LRESULT {
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

unsafe fn on_destroy(hwnd: HWND) -> LRESULT {
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

/// WM_COMMAND (button clicks / menu picks / control notifications) and WM_NOTIFY (list
/// custom-draw, drag-reorder, tooltips) — plus the format list's context menu, which is
/// keyed off the same target control as the format list's other notifications.
unsafe fn on_command_or_notify_msg(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    match msg {
        WM_COMMAND => Some(on_command(hwnd, wparam)),
        // A footer SysLink or the banner tooltip is asking for its rotating text.
        WM_NOTIFY => Some(on_notify(hwnd, lparam)),
        // Right-click / Shift+F10 on the format list → bulk check/uncheck menu.
        WM_CONTEXTMENU
            if HWND(wparam.0 as *mut c_void)
                == GetDlgItem(Some(hwnd), ID_LIST).unwrap_or_default() =>
        {
            list::list_context_menu(HWND(wparam.0 as *mut c_void), hwnd, lparam);
            Some(LRESULT(0))
        }
        _ => None,
    }
}

unsafe fn on_command(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    let id = (wparam.0 & 0xFFFF) as i32;
    let notify = ((wparam.0 >> 16) & 0xFFFF) as u32;
    on_command_dialog(hwnd, id, notify);
    on_command_shot(hwnd, id);
    on_command_sync_nav(hwnd, id, notify);
    on_command_admin(hwnd, id);
    on_command_licence(hwnd, id);
    LRESULT(0)
}

/// Core dialog chrome: Save/Cancel, the file-type list's bulk toggles + search + reset,
/// and the menu-items list's reset/editor.
unsafe fn on_command_dialog(hwnd: HWND, id: i32, notify: u32) {
    match id {
        IDOK => {
            // Refuse the whole Save when two enabled hotkeys share a chord, naming both,
            // rather than writing a duplicate whose LATER-registered half will silently
            // never fire (2026-09-05 audit, F27). See `block_on_hotkey_conflict`.
            if !block_on_hotkey_conflict(hwnd) {
                apply_settings(hwnd); // Save = apply only, keep the window open
                spawn_sync_push(hwnd); // if signed in, mirror the change to the cloud
            }
        }
        IDCANCEL => close_settings(hwnd),
        ID_SELECT_ALL | ID_CLEAR_ALL => on_select_clear_all(hwnd, id),
        // Settings-wide search: filter on every keystroke, jump on pick.
        ID_SEARCH_GLOBAL if notify == EN_CHANGE => search::on_change(hwnd),
        ID_SEARCH_RESULTS if notify == LBN_SELCHANGE => search::on_pick(hwnd),
        ID_SEARCH if notify == EN_CHANGE => on_search_filter_changed(hwnd),
        ID_DEFAULTS => reset_formats(hwnd), // file-type list only (see its tip)
        ID_RESET_ALL => load_defaults(hwnd), // whole dialog → factory defaults
        ID_MENU_RESET => {
            if let Ok(mlist) = GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST) {
                list::reset_menu_order(mlist);
            }
        }
        // The checklist itself lives in a popup editor now — room it never
        // had on the page, and the page gets its breathing space back.
        ID_MENU_ITEMS_EDIT => menuitems::open(hwnd),
        _ => {}
    }
}

/// Affects the currently-shown (filtered) rows; the model
/// syncs via LVN_ITEMCHANGED, so off-screen formats are kept.
unsafe fn on_select_clear_all(hwnd: HWND, id: i32) {
    if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
        let on = id == ID_SELECT_ALL;
        let count = SendMessageW(list, LVM_GETITEMCOUNT, None, None).0 as i32;
        for i in 0..count {
            set_check(list, i, on);
        }
    }
}

unsafe fn on_search_filter_changed(hwnd: HWND) {
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

/// The screenshot-tool controls: instant-screenshot / quick-save / custom-action
/// enable toggles, the save-folder picker + toggle, and the restart button.
unsafe fn on_command_shot(hwnd: HWND, id: i32) {
    match id {
        // Instant-screenshot checkbox: enable/disable its hotkey picker live —
        // and re-grey its dependent rows (Quick screenshot / save-folder toggle).
        ID_SHOT_ENABLE => {
            refresh_shot_status(hwnd);
            sync_dependent_switches(hwnd);
        }
        ID_SHOT_QUICK_ENABLE => update_quick_enabled(hwnd),
        ID_CUSTOM_ACTION_ENABLE => update_custom_action_enabled(hwnd),
        ID_SHOT_USE_DIR => update_save_dir_enabled(hwnd),
        ID_SHOT_SET_DIR => on_shot_set_dir(hwnd),
        ID_SHOT_RESTART => on_shot_restart(hwnd),
        ID_EDIT_UPLOAD_HOSTS => crate::screenshot::open_hosts_config(),
        _ => {}
    }
}

/// Pick the Ctrl+S save folder; persist immediately + refresh the
/// display. (The toggle next to it is saved with the other settings
/// on the Save button.)
unsafe fn on_shot_set_dir(hwnd: HWND) {
    if let Some(dir) = crate::win::pick_folder(hwnd) {
        let _ = settings::set_screenshot_save_dir(&dir);
        set_shot_dir_label(hwnd);
    }
}

/// (Re)start the tray daemon: ensure the autostart entry + a
/// live daemon, then re-register the current hotkey. Tick the
/// Enable box to match, and show an optimistic status (the
/// daemon was just spawned; the true state shows on reopen).
unsafe fn on_shot_restart(hwnd: HWND) {
    crate::screenshot::set_enabled(true);
    crate::screenshot::reload_hotkey();
    check(hwnd, ID_SHOT_ENABLE, true);
    set_shot_status(hwnd, "Started", true);
    // check() above is a raw BM_SETCHECK, not a click: it never sends
    // WM_COMMAND, so the normal ID_SHOT_ENABLE handler (which greys/ungreys
    // ID_SHOT_QUICK_ENABLE / ID_SHOT_USE_DIR) never runs on its own here.
    sync_dependent_switches(hwnd);
}

/// Dependent-switch fan-out (menu rows, Quick-preview rows), the sync button, the
/// sign-in nudge card, the language combo, the nav rail, and the sponsor banner.
unsafe fn on_command_sync_nav(hwnd: HWND, id: i32, notify: u32) {
    match id {
        // Parent switches with greyed dependents (the menu rows, the Quick-preview
        // rows): one table drives them all — see `DEPENDENT_SWITCHES`.
        ID_ENABLE_MENU | ID_PREVIEW_ENABLED => sync_dependent_switches(hwnd),
        // The badge-style row's parent is a COMBO, not a checkbox, so it arrives as
        // a selection change rather than a click — see `DEPENDENT_ON_COMBO`.
        ID_CORNER_MARK if notify == CBN_SELCHANGE => sync_dependent_switches(hwnd),
        ID_SYNC_BTN => on_sync_click(hwnd),
        ID_NUDGE_ACTION | ID_NUDGE_LATER | ID_NUDGE_MONTH | ID_NUDGE_DISCORD => {
            nudge::on_command(hwnd, id);
        }
        ID_BIZNAG_ACTION | ID_BIZNAG_BUY => {
            biznag::on_command(hwnd, id);
        }
        ID_LANG if notify == CBN_SELCHANGE => on_lang_change(hwnd),
        nav if (ID_NAV_BASE..ID_NAV_BASE + NCAT as i32).contains(&nav) && notify == STN_CLICKED => {
            switch_category(hwnd, (nav - ID_NAV_BASE) as usize);
        }
        ID_BANNER if notify == STN_CLICKED => on_banner_click(hwnd),
        _ => {}
    }
}

/// Open the currently-shown sponsor's link (or the product page if no sponsor
/// feed loaded).
unsafe fn on_banner_click(hwnd: HWND) {
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

/// The admin/diagnostics buttons: About, log, import/export, cache rebuild,
/// association repair, the doctor report, portable registration, update check.
unsafe fn on_command_admin(hwnd: HWND, id: i32) {
    match id {
        ID_ABOUT => show_about(hwnd),
        ID_OPEN_LOG => open_diagnostics_log(),
        ID_EXPORT => export_settings_to_file(hwnd),
        ID_IMPORT => import_settings_from_file(hwnd),
        ID_REBUILD_CACHE => rebuild_thumbnail_cache(hwnd),
        ID_REPAIR_ASSOC => repair_associations(hwnd),
        // Owned modal, like the feedback box: Settings stays open behind it.
        ID_RUN_DOCTOR => crate::doctor_report::run_doctor_report(Some(hwnd)),
        ID_PORTABLE_REG => toggle_portable_registration(hwnd),
        ID_CHECK_UPDATES => show_about(hwnd),
        _ => {}
    }
}

/// The Licence page's two network buttons — Redeem and Check now. Both run on a worker
/// thread and post back through `WM_APP_LICENCE`; see `licence_ui.rs`.
unsafe fn on_command_licence(hwnd: HWND, id: i32) {
    match id {
        ID_LICENCE_REDEEM_BTN => licence_ui::on_redeem_click(hwnd),
        ID_LICENCE_CHECK_NOW => licence_ui::on_check_now_click(hwnd),
        ID_LICENCE_BUY => crate::win::open_url(crate::license::BUY_URL),
        ID_LICENCE_RENEW => crate::win::open_url(&crate::license::renew_url()),
        ID_LICENCE_MOVE => crate::win::open_url(crate::license::PORTAL_CLAIM_URL),
        _ => {}
    }
}

unsafe fn on_notify(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let nmhdr = lparam.0 as *const NMHDR;
    if let Some(r) = on_notify_begindrag(hwnd, nmhdr, lparam) {
        return r;
    }
    // Dark-mode modern restyle: own the paint of the format list, the
    // push buttons and the checkboxes via NM_CUSTOMDRAW. Light mode
    // returns nothing here, so the native themed look is unchanged.
    if (*nmhdr).code == NM_CUSTOMDRAW {
        return on_notify_customdraw(hwnd, lparam);
    }
    if let Some(r) = on_notify_itemchanged(hwnd, nmhdr, lparam) {
        return r;
    }
    on_notify_link_or_tip(hwnd, nmhdr, lparam)
}

/// Drag-to-reorder the "Menu items" checklist: begin on LVN_BEGINDRAG.
unsafe fn on_notify_begindrag(hwnd: HWND, nmhdr: *const NMHDR, lparam: LPARAM) -> Option<LRESULT> {
    if (*nmhdr).code == windows::Win32::UI::Controls::LVN_BEGINDRAG
        && (*nmhdr).hwndFrom == GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST).unwrap_or_default()
    {
        let nmlv = lparam.0 as *const NMLISTVIEW;
        list::begin_menu_drag((*nmhdr).hwndFrom, (*nmlv).iItem);
        return Some(LRESULT(0));
    }
    None
}

unsafe fn on_notify_customdraw(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let nmhdr = lparam.0 as *const NMHDR;
    let from = (*nmhdr).hwndFrom;
    if from == GetDlgItem(Some(hwnd), ID_LIST).unwrap_or_default() {
        return LRESULT(restyle::draw_list_item(lparam.0 as *mut NMLVCUSTOMDRAW));
    }
    if is_button_class(from) {
        return LRESULT(restyle::draw_button_cd(
            hwnd,
            lparam.0 as *const NMCUSTOMDRAW,
        ));
    }
    // SysLink credit etc. — let it draw itself.
    LRESULT(CDRF_DODEFAULT as isize)
}

/// A FORMAT row's checkbox toggled → sync the model (FMT_STATE). Gate on
/// the source being the format list: the Menu-items checklist is also a
/// checkbox ListView and must NOT feed FMT_STATE (its state is read
/// directly in apply_settings).
unsafe fn on_notify_itemchanged(
    hwnd: HWND,
    nmhdr: *const NMHDR,
    lparam: LPARAM,
) -> Option<LRESULT> {
    if (*nmhdr).code != LVN_ITEMCHANGED
        || (*nmhdr).hwndFrom != GetDlgItem(Some(hwnd), ID_LIST).unwrap_or_default()
        || POPULATING.with(|p| p.get())
    {
        return None;
    }
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
    Some(LRESULT(0))
}

unsafe fn on_notify_link_or_tip(hwnd: HWND, nmhdr: *const NMHDR, lparam: LPARAM) -> LRESULT {
    let code = (*nmhdr).code;
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

/// Owner-draw + cursor messages: the dark context-menu items, the format-list /
/// nudge-card / nav-rail / section-header owner-draw statics, the banner's hand
/// cursor, and the double-buffered WM_PAINT.
unsafe fn on_paint_msg(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    match msg {
        // Owner-drawn dark context-menu items (light text on dark).
        WM_MEASUREITEM => Some(on_measureitem(hwnd, msg, wparam, lparam)),
        WM_DRAWITEM => Some(on_drawitem(hwnd, msg, wparam, lparam)),
        // Hand cursor over the clickable banner (so it reads as clickable).
        WM_SETCURSOR
            if HWND(wparam.0 as *mut c_void)
                == GetDlgItem(Some(hwnd), ID_BANNER).unwrap_or_default() =>
        {
            let _ = SetCursor(LoadCursorW(None, IDC_HAND).ok());
            Some(LRESULT(1))
        }
        // All background painting is owned by WM_PAINT (double-buffered below), so
        // suppress the default erase: returning 1 stops DefWindowProcW from filling
        // the invalid band with the class brush as a SEPARATE deferred frame — that
        // erase-then-paint two-step is the white/gray flash on a fast left scroll.
        WM_ERASEBKGND => Some(LRESULT(1)),
        WM_PAINT => Some(on_paint(hwnd)),
        _ => None,
    }
}

unsafe fn on_measureitem(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let m = &mut *(lparam.0 as *mut MEASUREITEMSTRUCT);
    if m.CtlID == ID_SEARCH_RESULTS as u32 {
        search::measure_row(hwnd, m);
        return LRESULT(1);
    }
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

unsafe fn on_drawitem(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let d = &*(lparam.0 as *const DRAWITEMSTRUCT);
    if d.CtlID == ID_SEARCH_RESULTS as u32 {
        search::draw_row(hwnd, d);
        return LRESULT(1);
    }
    if d.CtlType == ODT_MENU {
        return on_drawitem_menu(d);
    }
    if d.CtlType == ODT_STATIC {
        on_drawitem_static(hwnd, d);
        return LRESULT(1);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe fn on_drawitem_menu(d: &DRAWITEMSTRUCT) -> LRESULT {
    let selected = (d.itemState.0 & ODS_SELECTED.0) != 0;
    let bg = if selected {
        dark_menu_sel_brush()
    } else {
        dark_menu_brush()
    };
    FillRect(d.hDC, &d.rcItem, bg);
    SetBkMode(d.hDC, TRANSPARENT);
    SetTextColor(d.hDC, DARK_TEXT());
    SelectObject(d.hDC, HGDIOBJ(gui_font().0));
    let mut label = wide(list::ctx_menu_label(d.itemID as usize));
    let n = label.len().saturating_sub(1);
    let mut rc = d.rcItem;
    rc.left += 14;
    DrawTextW(
        d.hDC,
        &mut label[..n],
        &mut rc,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    LRESULT(1)
}

unsafe fn on_drawitem_static(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    let cid = d.CtlID as i32;
    if d.CtlID == ID_LEFT_MASK as u32 {
        scroll::draw_left_mask(hwnd, d);
    } else if cid == ID_NUDGE_CARD {
        nudge::draw_card(hwnd, d);
    } else if cid == ID_BIZNAG_CARD {
        biznag::draw_card(hwnd, d);
    } else if cid == ID_PANE_HEADER {
        draw_pane_header(hwnd, d);
    } else if (ID_NAV_BASE..ID_NAV_BASE + NCAT as i32).contains(&cid) {
        let active = NAV.with(|n| n.borrow().active) == (cid - ID_NAV_BASE) as usize;
        draw_nav_item(hwnd, d, active);
    } else {
        // The owner-drawn section headers (uppercase label + divider).
        restyle::draw_section_header(hwnd, d);
    }
}

/// Paint the dialog background + the "chrome" (rounded list card / input +
/// dropdown field frames behind their controls / hairline dividers) into an
/// off-screen buffer, then blit once — so the fill and the chrome land in the
/// SAME frame instead of flashing the bare background between them. The blit is
/// clipped to non-child pixels by WS_CLIPCHILDREN, so the child controls keep
/// their own (SetWindowPos-preserved) pixels and aren't briefly overpainted.
/// The fill brush MIRRORS the class hbrBackground (main.rs) exactly so light
/// mode is byte-identical to before (COLOR_BTNFACE, not the 243 surface tone).
unsafe fn on_paint(hwnd: HWND) -> LRESULT {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let pr = ps.rcPaint;
    let (pw, ph) = (pr.right - pr.left, pr.bottom - pr.top);
    if pw > 0 && ph > 0 {
        let br = if is_dark() {
            dark_bg_brush()
        } else {
            HBRUSH(16isize as *mut c_void)
        };
        let mem = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, pw, ph);
        // Under GDI handle exhaustion either call can come back invalid, so mirror
        // preview/paint.rs's fallback rather than blitting from an unset default
        // bitmap (which would paint garbage/black instead of the real chrome).
        if !mem.is_invalid() && !bmp.is_invalid() {
            let old = SelectObject(mem, HGDIOBJ(bmp.0));
            // Map client coords onto the dirty-rect-sized buffer so paint_chrome
            // (which works in client coords) draws into the right place.
            let _ = SetViewportOrgEx(mem, -pr.left, -pr.top, None);
            FillRect(mem, &pr, br);
            restyle::paint_chrome(hwnd, mem);
            let _ = SetViewportOrgEx(mem, 0, 0, None);
            let _ = BitBlt(hdc, pr.left, pr.top, pw, ph, Some(mem), 0, 0, SRCCOPY);
            SelectObject(mem, old);
        } else {
            // Buffer alloc failed: paint straight to the window DC (correct,
            // just flickers on a fast scroll instead of blitting garbage).
            FillRect(hdc, &pr, br);
            restyle::paint_chrome(hwnd, hdc);
        }
        if !bmp.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(bmp.0));
        }
        if !mem.is_invalid() {
            let _ = DeleteDC(mem);
        }
    }
    let _ = EndPaint(hwnd, &ps);
    LRESULT(0)
}

/// The three WM_TIMER chords (status refresh / GIF frame advance / sponsor rotate)
/// plus the left-column scrollbar + mouse wheel.
unsafe fn on_timer_or_scroll_msg(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    match msg {
        // Keep the hotkey-service status line honest while the dialog is open.
        WM_TIMER if wparam.0 == TIMER_SHOT_STATUS => {
            refresh_shot_status(hwnd);
            Some(LRESULT(0))
        }
        WM_TIMER if wparam.0 == TIMER_BANNER => Some(on_timer_banner(hwnd)),
        WM_TIMER if wparam.0 == TIMER_ROTATE => Some(on_timer_rotate(hwnd)),
        // Left-column scrolling (dark mode): the scrollbar + the mouse wheel.
        WM_VSCROLL => {
            scroll::on_vscroll(hwnd, wparam, lparam);
            Some(LRESULT(0))
        }
        WM_MOUSEWHEEL => {
            let wheel = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let pos = scroll::SCROLL.with(|s| s.borrow().pos);
            scroll::scroll_to(hwnd, pos - wheel / 120 * dpi_scale(hwnd, 42));
            Some(LRESULT(0))
        }
        _ => None,
    }
}

/// Advance the current image's GIF animation one frame (frames are reused
/// each loop, so don't free the prior one; WM_DESTROY frees them all).
unsafe fn on_timer_banner(hwnd: HWND) -> LRESULT {
    if let Some((banner, rot)) = banner_rotator(hwnd) {
        let r = &mut *rot;
        let (cur, imgi) = (r.cur, r.img);
        let nframes = r
            .sponsors
            .get(cur)
            .and_then(|a| a.images.get(imgi))
            .map_or(0, |im| im.frames.len());
        if nframes > 1 {
            r.frame = (r.frame + 1) % nframes;
            let f = r.sponsors[cur].images[imgi].frames[r.frame];
            SendMessageW(
                banner,
                STM_SETIMAGE,
                Some(WPARAM(IMAGE_BITMAP.0 as usize)),
                Some(LPARAM(f)),
            );
        }
    }
    LRESULT(0)
}

/// Rotate to the next sponsor / image: advance the rotator, then show the
/// new art (raw STM_SETIMAGE so the prior bitmap survives — the rotator
/// still owns it). The tooltip pulls the fresh text on the next hover.
unsafe fn on_timer_rotate(hwnd: HWND) -> LRESULT {
    if let Some((banner, rot)) = banner_rotator(hwnd) {
        (*rot).advance();
        show_current_image(hwnd, banner, &*rot, false);
    }
    LRESULT(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A093/A264: `sponsor_layout` used to open a wider gap above the footer
    /// (`foot_y` 534 instead of 470) whenever `sponsors_enabled()` was true, to
    /// reserve room for the banner it was about to create. That reservation is
    /// pointless now that `build_controls` never creates ID_BANNER in the first
    /// place (`navrail::V3_ALWAYS_HIDDEN` hides it on every page with no page
    /// that un-hides it), so the footer position must be the single fixed
    /// no-banner value regardless — this guards against a future edit
    /// reintroducing a sponsor-state-dependent gap without also reinstating a
    /// way to show the banner.
    #[test]
    fn sponsor_layout_never_reserves_room_for_the_never_shown_banner() {
        let layout = sponsor_layout(true);
        assert_eq!(
            layout.foot_y, 470,
            "footer must use the fixed no-banner spacing; ID_BANNER is never created"
        );
        assert_eq!(layout.credit_y, layout.foot_y + 6);
    }

    // ---- IDOK source-contract guard (2026-09-05 audit, F27 follow-up) ------------------

    /// `mod.rs` verbatim, embedded at compile time - same reasoning as `sync_client.rs`'s
    /// `SETTINGS_SRC`: `include_str!` resolves relative to this file and is checked by the
    /// compiler, so the scan below never depends on the working directory a test happens to
    /// run from.
    const MOD_SRC: &str = include_str!("mod.rs");

    /// The `values::hotkey_conflict_decision`/`block_on_hotkey_conflict` tests prove the
    /// CONFLICT MATH is right. Nothing proved the IDOK arm in `on_command_dialog` actually
    /// calls it, or calls it in the right order - an inverted `if !block_on_hotkey_conflict
    /// (hwnd)` (dropping the `!`), or a reordering that ran `apply_settings`/
    /// `spawn_sync_push` unconditionally, would compile clean and pass every other test in
    /// this repo. This is a dumb textual scan on the IDOK arm's own source, in the same
    /// spirit as `sync_client.rs::settings_in_source` and `navrail`'s measurement tests: it
    /// does not understand Rust, it just checks the names appear in the order Save requires.
    #[test]
    fn idok_arm_blocks_on_hotkey_conflict_before_apply_settings() {
        let start = MOD_SRC
            .find("IDOK => {")
            .expect("IDOK arm not found in mod.rs source - did on_command_dialog change shape?");
        let end = MOD_SRC[start..]
            .find("IDCANCEL =>")
            .map(|i| start + i)
            .expect("IDCANCEL arm not found after IDOK - on_command_dialog's match changed shape");
        let arm = &MOD_SRC[start..end];

        let guard_at = arm.find("if !block_on_hotkey_conflict(hwnd)").expect(
            "IDOK arm no longer guards Save on `if !block_on_hotkey_conflict(hwnd)` - the \
             call or its `!` negation may have been dropped, which would silently let Save \
             write a conflicting hotkey chord again",
        );
        let apply_at = arm
            .find("apply_settings(hwnd)")
            .expect("IDOK arm no longer calls apply_settings");
        let spawn_at = arm
            .find("spawn_sync_push(hwnd)")
            .expect("IDOK arm no longer calls spawn_sync_push");

        assert!(
            guard_at < apply_at,
            "block_on_hotkey_conflict must be checked BEFORE apply_settings in the IDOK arm"
        );
        assert!(
            guard_at < spawn_at,
            "block_on_hotkey_conflict must be checked BEFORE spawn_sync_push in the IDOK arm"
        );
        assert!(
            apply_at < spawn_at,
            "apply_settings must run before spawn_sync_push in the IDOK arm"
        );
    }
}
