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
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreatePen, DeleteDC,
    DeleteObject, DrawTextW, EndPaint, FillRect, GetDC,
    GetStockObject, GetTextExtentPoint32W, InvalidateRect, Polyline, RedrawWindow, ReleaseDC,
    RoundRect, ScreenToClient, SelectObject, SetBkMode, SetDCBrushColor, SetDCPenColor,
    SetTextCharacterExtra, SetTextColor, SetViewportOrgEx, DC_BRUSH, DC_PEN, DT_CENTER,
    DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HBRUSH, HDC, HGDIOBJ,
    PAINTSTRUCT, PS_SOLID, RDW_ALLCHILDREN, RDW_INVALIDATE, RDW_UPDATENOW, SRCCOPY, TRANSPARENT,
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

// ---- Control IDs --------------------------------------------------------
const ID_ENABLE_THUMBS: i32 = 1001;
const ID_USE_EMBEDDED: i32 = 1002;
const ID_ENABLE_MENU: i32 = 1003;
const ID_MAXSIZE: i32 = 1004;
const ID_SIZE: i32 = 1005;
const ID_JPEG: i32 = 1006;
const ID_PNG: i32 = 1007;
const ID_LIST: i32 = 1008;
const ID_SELECT_ALL: i32 = 1009;
const ID_CLEAR_ALL: i32 = 1010;
const ID_DEFAULTS: i32 = 1011;
// Translatable static labels (need IDs so the language picker can relabel live).
const ID_LBL_THUMBS: i32 = 1100;
const ID_LBL_LIMITS: i32 = 1101;
const ID_LBL_MAXFILE: i32 = 1102;
const ID_LBL_MAXTHUMB: i32 = 1103;
const ID_LBL_JPEG: i32 = 1104;
const ID_LBL_PNG: i32 = 1105;
const ID_LBL_FORMATS: i32 = 1106;
const ID_LBL_LANG: i32 = 1107;
const ID_LANG: i32 = 1108;
// Ebook/comic archive cover options.
const ID_LBL_EBOOK: i32 = 1109;
const ID_C_SORT: i32 = 1110;
const ID_C_PREFER_COVER: i32 = 1111;
const ID_C_SKIP_SCAN: i32 = 1112;
// Sponsor promotion (footer link + clickable banner + About box).
const ID_ABOUT: i32 = 1113;
const ID_PROMO_LINK: i32 = 1114;
const ID_BANNER: i32 = 1115;
// Context-menu preview placement (Off / submenu / main menu).
const ID_LBL_PREVIEW: i32 = 1116;
const ID_MENU_PREVIEW: i32 = 1117;
// Quick verbs directly on the main right-click menu.
const ID_MENU_QUICK: i32 = 1118;
// Show the menu on ALL file types (a condensed file-utility set on unsupported files).
const ID_MENU_ALL_TYPES: i32 = 1119;
// Subtle checkerboard behind the menu preview's transparent areas.
const ID_MENU_CHECKER: i32 = 1120;
// "Keep original date on saved files" — preserve source mtime on Convert/Resize/Rotate output.
const ID_PRESERVE_DATE: i32 = 1121;

// Settings-sync (optional Connections account) — the opt-in row. IDs 1200-1202 are free
// (control IDs stop at 1187; nav IDs start at 1700).
const ID_LBL_SYNC: i32 = 1200;
const ID_SYNC_BTN: i32 = 1201;
// Live "● Synced · up to date" status line beside the sync button (green when signed in,
// muted when signed out) — replaces baking the raw account id into the button label.
const ID_SYNC_STATUS: i32 = 1202;
// Left-column scroll plumbing: a vertical scrollbar + an opaque mask that hides
// controls scrolled below the viewport (so the left options can grow/scroll
// without making the window taller).
const ID_SCROLLBAR: i32 = 1131;
const ID_LEFT_MASK: i32 = 1132;
// Live search box that filters the supported-file-types list.
const ID_SEARCH: i32 = 1133;
// Screenshot capture service: an enable toggle + a hotkey preset picker (the
// opt-in tray daemon's global hotkey, configurable here instead of via the tray).
const ID_LBL_SHOT: i32 = 1134;
const ID_SHOT_ENABLE: i32 = 1135;
const ID_LBL_SHOT_HK: i32 = 1136;
const ID_SHOT_HOTKEY: i32 = 1137;
// Live daemon status line + a Start/Restart button (the hotkey only fires while the
// tray daemon is alive; this surfaces its state + lets you recover it).
const ID_SHOT_STATUS: i32 = 1139;
const ID_SHOT_RESTART: i32 = 1140;
// Settings checkbox: hide the daemon's notification-area (tray) icon.
const ID_SHOT_HIDE_TRAY: i32 = 1141;
// Optional second "quick-save" hotkey (full-screen → clipboard+PNG, no editor):
// an enable checkbox that gates the hotkey-picker combo.
const ID_SHOT_QUICK_ENABLE: i32 = 1144;
const ID_LBL_SHOT_QUICK_HK: i32 = 1142;
const ID_SHOT_QUICK_HOTKEY: i32 = 1143;
// Ctrl+S save destination: a "use a fixed folder" toggle, a folder-picker button, and a
// read-only display of the current folder (the Desktop known folder by default).
const ID_SHOT_USE_DIR: i32 = 1169;
const ID_SHOT_SET_DIR: i32 = 1170;
const ID_SHOT_DIR: i32 = 1171;
// "General" section header (right-click-menu settings + UI language).
const ID_LBL_GENERAL: i32 = 1138;
// "Menu items" checklist header (per-item context-menu visibility).
const ID_LBL_MENU_ITEMS: i32 = 1164;
// The "Menu items" visibility checklist — a compact checkbox ListView (like the
// Supported File Types list) instead of ~14 stacked checkboxes.
const ID_MENU_ITEMS_LIST: i32 = 1165;
// "Reset order" button under the checklist — restores the default drag-reorder order.
const ID_MENU_RESET: i32 = 1145;
// "Reset all settings" button (left column, end of Diagnostics) — factory reset of the
// whole dialog. (The top-right "Defaults" resets only the file-type list — see its tip.)
const ID_RESET_ALL: i32 = 1146;

// Diagnostics section (error/crash log).
const ID_LBL_DIAG: i32 = 1166;
const ID_VERBOSE_LOG: i32 = 1167;
const ID_OPEN_LOG: i32 = 1168;
// Import / Export settings — they share the Reset row at the end of Diagnostics
// (1169–1171 are the Ctrl+S save-dir controls above).
const ID_IMPORT: i32 = 1172;
const ID_EXPORT: i32 = 1173;
// Diagnostics actions: clear Windows' thumbnail cache + check GitHub for a newer release.
const ID_REBUILD_CACHE: i32 = 1174;
const ID_CHECK_UPDATES: i32 = 1175;
// Re-register all enabled formats (fixes thumbnails stolen by another app).
const ID_REPAIR_ASSOC: i32 = 1176;
// Toggle the background update check (the one the resident hotkey helper runs).
const ID_UPDATE_AUTO: i32 = 1177;
// Custom action hotkey (the user-assignable "action -> hotkey" binding): an action
// picker + a hotkey-chord picker, both under the Screenshots section.
const ID_LBL_SHOT_ACTION: i32 = 1178;
const ID_SHOT_ACTION: i32 = 1179;
const ID_LBL_SHOT_ACTION_HK: i32 = 1180;
const ID_SHOT_ACTION_HK: i32 = 1181;
// v3 reorg: an explicit enable toggle for the custom action (gates the two combos),
// plus group sub-headers for the reorganized General / Advanced pages.
const ID_CUSTOM_ACTION_ENABLE: i32 = 1182;
const ID_LBL_UPDATES: i32 = 1184;
const ID_LBL_BACKUP: i32 = 1185;
const ID_LBL_HOTKEY_SVC: i32 = 1186;
// "Edit upload hosts…" — opens the user-editable upload-hosts config file
// (%APPDATA%\SageThumbs2K\upload-hosts.conf) in the default text editor.
const ID_EDIT_UPLOAD_HOSTS: i32 = 1187;

/// Per-item menu-visibility checkboxes (XnShell-style "Displayed menu items").
/// Each (control id, MENU title key); the checkbox LABEL reuses the menu item's
/// own translated name via `t(key)`. `menu_settings` is intentionally absent — the
/// Settings entry is always shown so the dialog stays reachable.
const MENU_ITEM_TOGGLES: &[(i32, &str)] = &[
    (1150, "menu_convert_into"),
    (1151, "menu_convert_dialog"),
    (1152, "menu_combine_pdf"),
    (1153, "menu_combine_cbz"),
    (1154, "menu_resize"),
    (1155, "menu_email"),
    (1156, "menu_rotate"),
    (1157, "menu_rename"),
    (1158, "menu_files_to_folder"),
    (1159, "menu_sort"),
    // "Tools" is now four individually-toggleable top-level entries (was one submenu).
    (1160, "menu_copy_text"),
    (1147, "menu_image_info"),
    (1148, "menu_pick_color"),
    (1149, "menu_strip_meta"),
    (1161, "menu_copy"),
    (1183, "menu_upload"),
    (1162, "menu_set_folder_icon"),
    (1163, "menu_wallpaper"),
];

/// Capture-hotkey presets offered in the Settings dropdown, each paired with its
/// packed HOTKEYF/VK value (high byte = HOTKEYF_* modifiers, low byte = virtual
/// key) — the same packing `settings::screenshot_hotkey` stores. Curated to safe,
/// non-conflicting chords (no bare letters that would hijack a global key, and
/// avoiding Win+Shift+S / Alt+PrtScn which the OS already claims).
pub(crate) const SHOT_PRESETS: &[(&str, u32)] = &[
    ("Ctrl + PrtScn", (0x02 << 8) | 0x2C),
    ("PrtScn", 0x2C),
    ("Ctrl + Shift + S", ((0x02 | 0x01) << 8) | 0x53),
    ("Ctrl + Shift + A", ((0x02 | 0x01) << 8) | 0x41),
    ("Ctrl + Shift + 4", ((0x02 | 0x01) << 8) | 0x34),
    ("Ctrl + Alt + S", ((0x02 | 0x04) << 8) | 0x53),
    ("F9", 0x78),
    ("Ctrl + F12", (0x02 << 8) | 0x7B),
];
/// Default chord pre-selected in the quick-save combo when none is saved yet —
/// deliberately NOT the main `Ctrl + PrtScn` default, so enabling the instant
/// screenshot doesn't try to grab a chord already owned by the editor hotkey.
const QUICK_DEFAULT_LABEL: &str = "Ctrl + Shift + S";
/// EM_SETCUEBANNER (placeholder text for the search edit) — not in this metadata.
const EM_SETCUEBANNER: u32 = 0x1501;
/// Dropdown-list width for the language combo (wider than the closed box).
const CB_SETDROPPEDWIDTH: u32 = 0x0160;

// Left-column scroll geometry (96-dpi design px). The viewport is the visible
// band of the left options; content taller than it scrolls.
const LEFT_VIEW_TOP: i32 = 6;
const LEFT_VIEW_BOTTOM: i32 = 442;
const LEFT_RIGHT_EDGE: i32 = 340; // x past which a control is "right column" (not scrolled)

#[derive(Clone, Copy)]
struct SponsorLayout {
    banner_y: i32,
    foot_y: i32,
    credit_y: i32,
}

fn sponsor_layout(_dark: bool, sponsors_on: bool) -> SponsorLayout {
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
struct LeftCol {
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
fn menu_rows_from_tokens(tokens: &[String], check: impl Fn(usize) -> bool) -> Vec<(isize, bool)> {
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
fn saved_menu_rows() -> Vec<(isize, bool)> {
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
unsafe fn menu_row_toggle(list: HWND, row: i32) -> Option<usize> {
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
unsafe fn menu_row_param(list: HWND, row: i32) -> isize {
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

unsafe fn build_controls(hwnd: HWND, hinst: HINSTANCE) {
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

// ===================== v3 layout: nav rail + content pane =====================
// Geometry (96-dpi design px). The window is nav rail (left) + a content pane that
// shows ONE category at a time; the rest of the controls are hidden.
const NAV_X: i32 = 8;
const NAV_TOP: i32 = 14;
const NAV_W: i32 = 188;
const NAV_ITEM_H: i32 = 38;
const PANE_X: i32 = 212;
const PANE_W: i32 = 528;
const PANE_TOP: i32 = 16;
const PANE_HEAD_H: i32 = 50; // the icon-chip + title + blurb page header
const ID_NAV_BASE: i32 = 1700; // nav items occupy ID_NAV_BASE .. ID_NAV_BASE+NCAT (1700..1707)
const ID_PANE_HEADER: i32 = 1710;
const NCAT: usize = 8;
/// Localized nav-rail / page-header label for category `ci`. Pulls from `t()` so a
/// live language switch re-texts it (the nav statics + pane header re-read this).
fn nav_label(ci: usize) -> &'static str {
    match ci {
        0 => t("nav_general"),
        1 => t("nav_filetypes"),
        2 => t("nav_ebook"),
        3 => t("nav_menu"),
        4 => t("nav_screenshots"),
        5 => t("nav_quickaction"),
        6 => t("nav_advanced"),
        _ => t("nav_databackup"),
    }
}

/// One row in a category's content pane. Ids reference controls already created by
/// `build_controls`; the layout just repositions them.
#[derive(Clone, Copy)]
enum Row {
    Head(i32),               // group sub-header (owner-draw static)
    Switch(i32),             // a checkbox, drawn as a toggle switch
    Pair(i32, i32, i32, i32),// label, field, field_w, field_h (combo field_h>40)
    Btn(i32, i32),           // button id, width
    BtnStatus(i32, i32, i32),// button (left) + right-aligned status, one row: btn_id, btn_w, status_id
    StatusBtn(i32, i32, i32),// status (left, fills) + right-aligned button, one row: status_id, btn_id, btn_w
    Status(i32),             // dynamic status line
    Btn3(i32, i32, i32),     // three equal buttons on one row
    Wide(i32),               // a full-width control (search edit)
    ListFill(i32),           // a list that fills down to the footer
    ListAuto(i32),           // a list that keeps its measured height
}

// Category order: General (Thumbnails+General merged) · File types · Ebook/comic ·
// Right-click menu · Screenshots · Advanced.
fn cat_rows(ci: usize) -> &'static [Row] {
    use Row::*;
    match ci {
        0 => &[
            // General = the merged Thumbnails + General (Custom action is its own tab now).
            Switch(ID_ENABLE_THUMBS), Switch(ID_USE_EMBEDDED), Switch(ID_MENU_CHECKER),
            Head(ID_LBL_LIMITS),
            Pair(ID_LBL_MAXFILE, ID_MAXSIZE, 84, 18), Pair(ID_LBL_MAXTHUMB, ID_SIZE, 84, 18),
            Pair(ID_LBL_JPEG, ID_JPEG, 84, 18), Pair(ID_LBL_PNG, ID_PNG, 84, 18),
            Head(ID_LBL_GENERAL), // "Language & files"
            Pair(ID_LBL_LANG, ID_LANG, 156, 200), Switch(ID_PRESERVE_DATE),
        ],
        1 => &[
            Btn3(ID_SELECT_ALL, ID_CLEAR_ALL, ID_DEFAULTS),
            Wide(ID_SEARCH),
            ListFill(ID_LIST),
        ],
        2 => &[
            // Ebook/comic — its own tab now.
            Switch(ID_C_SORT), Switch(ID_C_PREFER_COVER), Switch(ID_C_SKIP_SCAN),
        ],
        3 => &[
            Switch(ID_ENABLE_MENU), Switch(ID_MENU_ALL_TYPES), Switch(ID_MENU_QUICK),
            Pair(ID_LBL_PREVIEW, ID_MENU_PREVIEW, 156, 200),
            Head(ID_LBL_MENU_ITEMS),
            ListAuto(ID_MENU_ITEMS_LIST), Btn(ID_MENU_RESET, 110),
        ],
        4 => &[
            // Screenshots — custom action moved to General; hotkey service + "Hide tray icon" to Advanced.
            Switch(ID_SHOT_ENABLE), Switch(ID_SHOT_QUICK_ENABLE), Switch(ID_SHOT_USE_DIR),
            Pair(ID_LBL_SHOT_HK, ID_SHOT_HOTKEY, 156, 200),
            Pair(ID_LBL_SHOT_QUICK_HK, ID_SHOT_QUICK_HOTKEY, 156, 200),
            Status(ID_SHOT_DIR), Btn(ID_SHOT_SET_DIR, 150),
            Btn(ID_EDIT_UPLOAD_HOSTS, 184),
        ],
        5 => &[
            // Quick action — bind a global hotkey to run a tool.
            Switch(ID_CUSTOM_ACTION_ENABLE),
            Pair(ID_LBL_SHOT_ACTION, ID_SHOT_ACTION, 156, 200),
            Pair(ID_LBL_SHOT_ACTION_HK, ID_SHOT_ACTION_HK, 156, 200),
        ],
        6 => &[
            // Advanced — system behaviors only: Diagnostics / Updates / Hotkey service.
            // (Settings sync + Backup moved to their own "Data & Backup" tab.)
            Head(ID_LBL_DIAG),
            Switch(ID_VERBOSE_LOG),
            Btn(ID_OPEN_LOG, 320),
            Btn(ID_REBUILD_CACHE, 320),
            Btn(ID_REPAIR_ASSOC, 320),
            Head(ID_LBL_UPDATES),
            Switch(ID_UPDATE_AUTO), Btn(ID_CHECK_UPDATES, 184),
            Head(ID_LBL_HOTKEY_SVC),
            BtnStatus(ID_SHOT_RESTART, 184, ID_SHOT_STATUS), Switch(ID_SHOT_HIDE_TRAY),
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

#[derive(Default)]
struct NavState {
    active: usize,
    cats: Vec<Vec<HWND>>,
}
thread_local! {
    static NAV: std::cell::RefCell<NavState> = std::cell::RefCell::new(NavState::default());
}

/// Mix `pct`% of `fg` over `bg` (both 0x00BBGGRR COLORREFs) — for accent tints.
fn blend(fg: COLORREF, bg: COLORREF, pct: i32) -> COLORREF {
    let ch = |sh: u32| {
        let a = ((fg.0 >> sh) & 0xFF) as i32;
        let b = ((bg.0 >> sh) & 0xFF) as i32;
        (((a * pct + b * (100 - pct)) / 100) as u32) & 0xFF
    };
    COLORREF(ch(0) | (ch(8) << 8) | (ch(16) << 16))
}

/// Draw a category's line icon (matching the v3 web SVGs) in an `sz`×`sz` box at
/// `(x, y)`, stroked in `color`. Hollow shapes, ~1.5px stroke — a Fluent line look.
unsafe fn draw_cat_icon(hdc: HDC, ci: usize, x: i32, y: i32, sz: i32, color: COLORREF) {
    use windows::Win32::Graphics::Gdi::Ellipse;
    let pw = (sz / 12).max(1);
    let pen = CreatePen(PS_SOLID, pw, color);
    let oldp = SelectObject(hdc, HGDIOBJ(pen.0));
    let oldb = SelectObject(hdc, GetStockObject(windows::Win32::Graphics::Gdi::NULL_BRUSH));
    // Map the 24-unit SVG space into the box: x-coords via mx, y-coords via my.
    let mx = |v: i32| x + v * sz / 24;
    let my = |v: i32| y + v * sz / 24;
    let pt = |a: i32, b: i32| POINT { x: mx(a), y: my(b) };
    match ci {
        0 => {
            // image: framed rect + sun + mountain
            let _ = RoundRect(hdc, mx(3), my(3), mx(21), my(21), sz / 4, sz / 4);
            let _ = Ellipse(hdc, mx(6), my(6), mx(11), my(11));
            let _ = Polyline(hdc, &[pt(21, 15), pt(16, 10), pt(5, 21)]);
        }
        1 => {
            // grid: four rounded squares
            for (gx, gy) in [(3, 3), (13, 3), (3, 13), (13, 13)] {
                let _ = RoundRect(hdc, mx(gx), my(gy), mx(gx + 8), my(gy + 8), sz / 8, sz / 8);
            }
        }
        2 => {
            // book: cover + spine + page lines (Ebook/comic)
            let _ = RoundRect(hdc, mx(5), my(4), mx(19), my(20), sz / 8, sz / 8);
            let _ = Polyline(hdc, &[pt(8, 4), pt(8, 20)]);
            let _ = Polyline(hdc, &[pt(11, 9), pt(16, 9)]);
            let _ = Polyline(hdc, &[pt(11, 13), pt(16, 13)]);
        }
        3 => {
            // menu: three lines (last shorter)
            for (yy, x2) in [(6, 20), (12, 20), (18, 14)] {
                let _ = Polyline(hdc, &[pt(4, yy), pt(x2, yy)]);
            }
        }
        4 => {
            // camera: body + bump + lens
            let _ = RoundRect(hdc, mx(3), my(8), mx(21), my(19), sz / 8, sz / 8);
            let _ = Polyline(hdc, &[pt(8, 8), pt(9, 6), pt(15, 6), pt(16, 8)]);
            let _ = Ellipse(hdc, mx(9), my(10), mx(15), my(16));
        }
        5 => {
            // bolt: a lightning shape (Quick action)
            let _ = Polyline(hdc, &[pt(13, 2), pt(7, 13), pt(11, 13), pt(10, 22), pt(18, 10), pt(12, 10), pt(13, 2)]);
        }
        6 => {
            // sliders: two lines, each with a knob (Advanced)
            let _ = Polyline(hdc, &[pt(4, 8), pt(20, 8)]);
            let _ = Polyline(hdc, &[pt(4, 16), pt(20, 16)]);
            let _ = Ellipse(hdc, mx(13), my(5), mx(19), my(11));
            let _ = Ellipse(hdc, mx(5), my(13), mx(11), my(19));
        }
        _ => {
            // save/backup: a down-arrow into an open tray (Data & Backup)
            let _ = Polyline(hdc, &[pt(12, 3), pt(12, 14)]);
            let _ = Polyline(hdc, &[pt(8, 10), pt(12, 14), pt(16, 10)]);
            let _ = Polyline(hdc, &[pt(4, 16), pt(4, 21), pt(20, 21), pt(20, 16)]);
        }
    }
    SelectObject(hdc, oldp);
    SelectObject(hdc, oldb);
    let _ = DeleteObject(HGDIOBJ(pen.0));
}

fn cat_blurb(ci: usize) -> &'static str {
    match ci {
        0 => t("blurb_general"),
        1 => t("blurb_filetypes"),
        2 => t("blurb_ebook"),
        3 => t("blurb_menu"),
        4 => t("blurb_screenshots"),
        5 => t("blurb_quickaction"),
        6 => t("blurb_advanced"),
        _ => t("blurb_databackup"),
    }
}

/// Owner-draw a nav-rail item: an accent-tinted pill + accent icon + bar when
/// active; a muted icon + plain text otherwise.
unsafe fn draw_nav_item(hwnd: HWND, d: &DRAWITEMSTRUCT, active: bool) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    let ci = (d.CtlID as i32 - ID_NAV_BASE) as usize;
    fill(hdc, &rc, DARK_BG());
    if active {
        SelectObject(hdc, GetStockObject(DC_BRUSH));
        SelectObject(hdc, GetStockObject(DC_PEN));
        let tint = blend(ACCENT(), DARK_BG(), 16);
        SetDCBrushColor(hdc, tint);
        SetDCPenColor(hdc, tint);
        let _ = RoundRect(
            hdc,
            rc.left + dpi_scale(hwnd, 4),
            rc.top + dpi_scale(hwnd, 3),
            rc.right - dpi_scale(hwnd, 4),
            rc.bottom - dpi_scale(hwnd, 3),
            dpi_scale(hwnd, 8),
            dpi_scale(hwnd, 8),
        );
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
    draw_cat_icon(hdc, ci, rc.left + dpi_scale(hwnd, 16), iy, isz, if active { ACCENT() } else { HEADER_TEXT() });
    SelectObject(hdc, HGDIOBJ(gui_font_for(hwnd).0));
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, DARK_TEXT());
    let mut label = control_text(d.hwndItem);
    let n = label.len().saturating_sub(1);
    let mut tr = RECT { left: rc.left + dpi_scale(hwnd, 44), ..rc };
    DrawTextW(hdc, &mut label[..n], &mut tr, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
}

/// Owner-draw the per-pane header: an accent-tinted icon chip + the active
/// category's bold title + a muted blurb (the v3 page-header look).
unsafe fn draw_pane_header(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    fill(hdc, &rc, DARK_BG());
    let ci = NAV.with(|n| n.borrow().active);
    let chip = dpi_scale(hwnd, 34);
    SelectObject(hdc, GetStockObject(DC_BRUSH));
    SelectObject(hdc, GetStockObject(DC_PEN));
    let tint = blend(ACCENT(), DARK_BG(), 16);
    SetDCBrushColor(hdc, tint);
    SetDCPenColor(hdc, tint);
    let _ = RoundRect(hdc, rc.left, rc.top, rc.left + chip, rc.top + chip, dpi_scale(hwnd, 9), dpi_scale(hwnd, 9));
    let isz = dpi_scale(hwnd, 18);
    draw_cat_icon(hdc, ci, rc.left + (chip - isz) / 2, rc.top + (chip - isz) / 2, isz, ACCENT());
    let tx = rc.left + dpi_scale(hwnd, 46);
    SelectObject(hdc, HGDIOBJ(crate::win::gui_font_title(hwnd).0));
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, DARK_TEXT());
    let mut title = wide(nav_label(ci));
    let tn = title.len().saturating_sub(1);
    let mut tr = RECT { left: tx, top: rc.top - dpi_scale(hwnd, 2), right: rc.right, bottom: rc.top + dpi_scale(hwnd, 24) };
    DrawTextW(hdc, &mut title[..tn], &mut tr, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX);
    SelectObject(hdc, HGDIOBJ(gui_font_for(hwnd).0));
    SetTextColor(hdc, HEADER_TEXT());
    let mut blurb = wide(cat_blurb(ci));
    let bn = blurb.len().saturating_sub(1);
    let mut br = RECT { left: tx, top: rc.top + dpi_scale(hwnd, 26), right: rc.right, bottom: rc.bottom };
    DrawTextW(hdc, &mut blurb[..bn], &mut br, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
}

/// Show category `ci`'s controls, hide the others, repaint the nav + pane.
unsafe fn switch_category(hwnd: HWND, ci: usize) {
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
    let _ = InvalidateRect(Some(hwnd), None, true);
}

unsafe fn apply_v3_layout(hwnd: HWND, hinst: HINSTANCE) {
    // Hide the old scrolling chrome + the headers the nav/page-header now title.
    // (ID_LBL_GENERAL is now a sub-header on the merged General page; ID_LBL_EBOOK is
    // orphaned — Ebook/comic is its own tab with no sub-header.)
    for id in [ID_LBL_THUMBS, ID_LBL_EBOOK, ID_LBL_SHOT, ID_LBL_FORMATS, ID_SCROLLBAR, ID_LEFT_MASK, ID_BANNER] {
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
            let _ = SetWindowPos(c, None, sc(x), sc(y), sc(w), sc(h), SWP_NOZORDER | SWP_NOACTIVATE);
            Some(c)
        } else {
            None
        }
    };

    // Nav rail.
    #[allow(clippy::needless_range_loop)] // i drives both the label index and position/id math
    for i in 0..NCAT {
        ctl(
            hwnd,
            STATIC,
            nav_label(i),
            WINDOW_STYLE(SS_OWNERDRAW | SS_NOTIFY),
            NAV_X,
            NAV_TOP + i as i32 * NAV_ITEM_H,
            NAV_W,
            NAV_ITEM_H,
            ID_NAV_BASE + i as i32,
            hinst,
        );
    }

    // Per-pane header (icon chip + bold category title + blurb), redrawn per active
    // category. Always visible; content sits below it.
    ctl(hwnd, STATIC, "", WINDOW_STYLE(SS_OWNERDRAW), PANE_X, PANE_TOP, PANE_W, PANE_HEAD_H, ID_PANE_HEADER, hinst);

    let mut cats: Vec<Vec<HWND>> = vec![Vec::new(); NCAT];
    #[allow(clippy::needless_range_loop)] // ci indexes cats AND is passed to cat_rows(ci)
    for ci in 0..NCAT {
        let mut y = PANE_TOP + PANE_HEAD_H + 8;
        let mut first = true;
        for &row in cat_rows(ci) {
            match row {
                Row::Head(id) => {
                    if !first {
                        y += 14;
                    }
                    if let Some(c) = place(id, PANE_X, y, PANE_W, 18) {
                        cats[ci].push(c);
                    }
                    y += 24;
                }
                Row::Switch(id) => {
                    if let Some(c) = place(id, PANE_X, y, PANE_W, 28) {
                        cats[ci].push(c);
                    }
                    y += 32;
                }
                Row::Pair(lbl, field, fw, fh) => {
                    let lbl_dy = if fh > 40 { 4 } else { 2 };
                    if let Some(c) = place(lbl, PANE_X, y + lbl_dy, 220, 18) {
                        cats[ci].push(c);
                    }
                    if let Some(c) = place(field, PANE_X + PANE_W - fw, y, fw, fh) {
                        cats[ci].push(c);
                    }
                    y += 34;
                }
                Row::Btn(id, w) => {
                    if let Some(c) = place(id, PANE_X, y, w, 26) {
                        cats[ci].push(c);
                    }
                    y += 32;
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
                    y += 32;
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
                    y += 32;
                }
                Row::Status(id) => {
                    if let Some(c) = place(id, PANE_X, y, PANE_W, 18) {
                        cats[ci].push(c);
                    }
                    y += 22;
                }
                Row::Btn3(a, b, c3) => {
                    let gap = 8;
                    let w = (PANE_W - 2 * gap) / 3;
                    for (i, id) in [a, b, c3].into_iter().enumerate() {
                        if let Some(c) = place(id, PANE_X + i as i32 * (w + gap), y, w, 26) {
                            cats[ci].push(c);
                        }
                    }
                    y += 34;
                }
                Row::Wide(id) => {
                    y += 8; // extra breathing room above (the search box read squished)
                    if let Some(c) = place(id, PANE_X, y, PANE_W, 24) {
                        cats[ci].push(c);
                    }
                    y += 24 + 12; // and below
                }
                Row::ListFill(id) => {
                    let h = (footer_y - 8 - y).max(60);
                    if let Some(c) = place(id, PANE_X, y, PANE_W, h) {
                        cats[ci].push(c);
                    }
                    y += h + 4;
                }
                Row::ListAuto(id) => {
                    let mut wr = RECT::default();
                    let measured = if let Ok(c) = GetDlgItem(Some(hwnd), id) {
                        let _ = GetWindowRect(c, &mut wr);
                        ((wr.bottom - wr.top) * 96 / dpi).max(40)
                    } else {
                        40
                    };
                    // Cap so the row(s) below (e.g. Reset order) + the footer stay
                    // on-screen; the list scrolls internally if its rows don't fit.
                    let avail = (footer_y - y - 8 - 36).max(80);
                    let cur_h = measured.min(avail);
                    if let Some(c) = place(id, PANE_X, y, PANE_W, cur_h) {
                        cats[ci].push(c);
                    }
                    y += cur_h + 6;
                }
            }
            first = false;
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
    switch_category(hwnd, 0);
}
// =================== end v3 layout ===================

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
unsafe fn add_tooltips(hwnd: HWND, hinst: HINSTANCE) {
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
unsafe fn refresh_tooltips(hwnd: HWND) {
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
unsafe fn insert_column(list: HWND, idx: i32, title: &str, cx: i32) {
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
unsafe fn set_subitem(list: HWND, row: i32, col: i32, text: &str) {
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
}

/// RAII guard: clears POPULATING on scope exit (even on unwind), so the
/// LVN_ITEMCHANGED → FMT_STATE sync can never be left silently disabled.
struct PopulateGuard;
impl Drop for PopulateGuard {
    fn drop(&mut self) {
        POPULATING.with(|p| p.set(false));
    }
}

/// Rebuild the list to show the formats matching `filter` (extension / category /
/// description, case-insensitive; empty = all), each row's checkbox from FMT_STATE.
unsafe fn populate_list(list: HWND, filter: &str) {
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
unsafe fn fit_columns(list: HWND) {
    let mut crc = RECT::default();
    let _ = GetClientRect(list, &mut crc);
    // 64 + 92 are the extension + category column widths.
    let descw = ((crc.right - crc.left) - 64 - 92).max(80);
    SendMessageW(list, LVM_SETCOLUMNWIDTH, Some(WPARAM(2)), Some(LPARAM(descw as isize)));
}

// ---- Localization helpers ----------------------------------------------

/// All shipped language codes (English first).
fn lang_codes() -> Vec<&'static str> {
    i18n::codes().collect()
}

/// Fill the language combo: item 0 = "follow system", then each language by its
/// native name. Selects the current override (or "system" if none).
unsafe fn fill_lang_combo(combo: HWND) {
    add_combo_string(combo, t("lang_system"));
    let current = settings::lang_override();
    let mut sel = 0i32;
    for (i, code) in lang_codes().iter().enumerate() {
        add_combo_string(combo, i18n::native_name(code));
        if current.as_deref() == Some(*code) {
            sel = (i + 1) as i32;
        }
    }
    SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(sel as usize)), None);
}

unsafe fn add_combo_string(combo: HWND, s: &str) {
    let w = wide(s);
    SendMessageW(combo, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
}

/// The language code selected in the combo, or None for "follow system".
unsafe fn selected_lang(hwnd: HWND) -> Option<&'static str> {
    let combo = GetDlgItem(Some(hwnd), ID_LANG).ok()?;
    let sel = SendMessageW(combo, CB_GETCURSEL, None, None).0;
    if sel <= 0 {
        None
    } else {
        lang_codes().get((sel - 1) as usize).copied()
    }
}

/// Live language preview: re-resolve the locale and re-label every control
/// (without persisting — persistence happens on OK).
unsafe fn on_lang_change(hwnd: HWND) {
    i18n::apply_override_or_system(selected_lang(hwnd));
    apply_labels(hwnd);
    // The search cache keys on the (now stale-language) needle; clear it so the next
    // EN_CHANGE re-filters instead of short-circuiting on an identical needle.
    LAST_FILTER.with(|f| *f.borrow_mut() = None);
}

/// Re-apply every translatable label in the active language (used after a live
/// language change). Edits/selections are preserved (we only set text).
unsafe fn apply_labels(hwnd: HWND) {
    set_window_title(hwnd);
    let pairs: &[(i32, &str)] = &[
        (ID_LBL_THUMBS, "grp_thumbnails"),
        (ID_ENABLE_THUMBS, "chk_enable_thumbs"),
        (ID_USE_EMBEDDED, "chk_prefer_embedded"),
        (ID_ENABLE_MENU, "chk_enable_menu"),
        (ID_LBL_PREVIEW, "lbl_menu_preview"),
        (ID_MENU_QUICK, "chk_menu_quick"),
        (ID_MENU_CHECKER, "chk_menu_checker"),
        (ID_LBL_LIMITS, "grp_limits"),
        (ID_LBL_MAXFILE, "lbl_max_file"),
        (ID_LBL_MAXTHUMB, "lbl_max_thumb"),
        (ID_LBL_JPEG, "lbl_jpeg"),
        (ID_LBL_PNG, "lbl_png"),
        (ID_LBL_EBOOK, "grp_ebook"),
        (ID_LBL_GENERAL, "grp_lang_files"),
        (ID_C_SORT, "chk_sort"),
        (ID_C_PREFER_COVER, "chk_prefer_cover"),
        (ID_C_SKIP_SCAN, "chk_skip_scanlation"),
        (ID_LBL_FORMATS, "lbl_formats"),
        (ID_SELECT_ALL, "btn_select_all"),
        (ID_CLEAR_ALL, "btn_clear_all"),
        (ID_DEFAULTS, "btn_defaults"),
        (ID_LBL_LANG, "lbl_language"),
        (ID_LBL_SHOT, "grp_screenshots"),
        (ID_SHOT_ENABLE, "chk_screenshot"),
        (ID_SHOT_HIDE_TRAY, "chk_hide_tray"),
        (ID_LBL_SHOT_HK, "lbl_shot_hotkey"),
        (ID_SHOT_QUICK_ENABLE, "chk_instant_screenshot"),
        (ID_LBL_SHOT_QUICK_HK, "lbl_shot_quick_hotkey"),
        (ID_PRESERVE_DATE, "chk_preserve_date"),
        (ID_LBL_MENU_ITEMS, "grp_menu_items"),
        (ID_MENU_ALL_TYPES, "chk_menu_all_types"),
        (ID_MENU_RESET, "btn_menu_reset"),
        (ID_SHOT_USE_DIR, "chk_shot_use_dir"),
        (ID_SHOT_SET_DIR, "btn_set_save_dir"),
        (ID_SHOT_RESTART, "btn_restart_hotkey"),
        (ID_LBL_SHOT_ACTION, "lbl_custom_action"),
        (ID_LBL_SHOT_ACTION_HK, "lbl_custom_action_hk"),
        (ID_CUSTOM_ACTION_ENABLE, "chk_custom_action"),
        (ID_LBL_DIAG, "grp_diagnostics"),
        (ID_VERBOSE_LOG, "chk_verbose_log"),
        (ID_OPEN_LOG, "btn_open_log"),
        (ID_REBUILD_CACHE, "btn_rebuild_cache"),
        (ID_REPAIR_ASSOC, "btn_repair_assoc"),
        (ID_UPDATE_AUTO, "chk_update_auto"),
        (ID_CHECK_UPDATES, "btn_check_updates"),
        (ID_LBL_UPDATES, "grp_updates"),
        (ID_LBL_BACKUP, "grp_backup"),
        (ID_LBL_HOTKEY_SVC, "grp_hotkey_svc"),
        (ID_RESET_ALL, "btn_reset_all"),
        (ID_IMPORT, "btn_import"),
        (ID_EXPORT, "btn_export"),
        (IDOK, "btn_ok"),
        (IDCANCEL, "btn_cancel"),
    ];
    for &(id, key) in pairs {
        set_dlg_text(hwnd, id, t(key));
    }
    // Re-text + repaint the owner-draw nav rail and the page header (they read their
    // labels from nav_label()/cat_blurb(), which now follow the active language).
    for i in 0..NCAT as i32 {
        set_dlg_text(hwnd, ID_NAV_BASE + i, nav_label(i as usize));
        if let Ok(nav) = GetDlgItem(Some(hwnd), ID_NAV_BASE + i) {
            let _ = InvalidateRect(Some(nav), None, true);
        }
    }
    if let Ok(ph) = GetDlgItem(Some(hwnd), ID_PANE_HEADER) {
        let _ = InvalidateRect(Some(ph), None, true);
    }
    // The "Menu items" checklist rows relabel from their own menu keys (single col).
    // Rows may be in a custom drag-reorder, so read each ROW's key from its lParam —
    // relabeling by fixed toggle index would scramble the labels after a reorder.
    if let Ok(mlist) = GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST) {
        let count = SendMessageW(mlist, LVM_GETITEMCOUNT, None, None).0 as i32;
        for row in 0..count {
            if let Some(ti) = menu_row_toggle(mlist, row) {
                set_subitem(mlist, row, 0, t(MENU_ITEM_TOGGLES[ti].1));
            }
        }
    }
    if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
        // Columns are Extension | Category | Description (matching build_controls).
        // The old code relabeled column 1 with the *description* header (wrong index)
        // and never touched column 2, so a live language switch left "Category"
        // English and "Description" stale — fixed: correct indices + all three.
        set_column_text(list, 0, t("col_extension"));
        set_column_text(list, 1, t("col_category"));
        set_column_text(list, 2, t("col_description"));
    }
    // The preview-placement combo holds translated items: rebuild, keep selection.
    if let Ok(prev) = GetDlgItem(Some(hwnd), ID_MENU_PREVIEW) {
        let sel = SendMessageW(prev, CB_GETCURSEL, None, None).0.max(0);
        SendMessageW(prev, CB_RESETCONTENT, None, None);
        for key in ["prev_off", "prev_submenu", "prev_main"] {
            let w = wide(t(key));
            SendMessageW(prev, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
        }
        SendMessageW(prev, CB_SETCURSEL, Some(WPARAM(sel as usize)), None);
    }
    // The hover hints were also baked in the old language — re-text them.
    refresh_tooltips(hwnd);
}

unsafe fn set_dlg_text(hwnd: HWND, id: i32, s: &str) {
    if let Ok(h) = GetDlgItem(Some(hwnd), id) {
        let w = wide(s);
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
    }
}

unsafe fn set_window_title(hwnd: HWND) {
    let title = format!("SageThumbs 2K \u{2014} {}", t("lbl_options"));
    let w = wide(&title);
    let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr()));
}

unsafe fn set_column_text(list: HWND, idx: i32, s: &str) {
    let w = wide(s);
    let mut col = LVCOLUMNW {
        mask: LVCF_TEXT,
        pszText: PWSTR(w.as_ptr() as *mut u16),
        ..Default::default()
    };
    SendMessageW(list, LVM_SETCOLUMNW, Some(WPARAM(idx as usize)), Some(LPARAM(&mut col as *mut _ as isize)));
}

/// Repeating timer that keeps the hotkey-service status line live while Settings is open, so
/// it reflects a self-heal / watchdog restart (or the daemon dying) without reopening the
/// dialog. IDs 1–2 are the sponsor banner timers (see [`crate::sponsors`]); this is the third.
const TIMER_SHOT_STATUS: usize = 3;

/// Populate every control from the persisted settings.
/// Set the screenshot status line's text.
unsafe fn set_shot_status(hwnd: HWND, txt: &str) {
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_SHOT_STATUS) {
        let w = wide(txt);
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
    }
}

/// Update the save-folder display (ID_SHOT_DIR) to the effective folder (the configured
/// one, or the Desktop default). Called on load and after the folder picker.
unsafe fn set_shot_dir_label(hwnd: HWND) {
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_SHOT_DIR) {
        let w = wide(&format!("Folder: {}", crate::screenshot::effective_save_dir()));
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
    }
}

/// Refresh the screenshot daemon status line from the live state.
unsafe fn refresh_shot_status(hwnd: HWND) {
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

unsafe fn load_values(hwnd: HWND) {
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
unsafe fn load_defaults(hwnd: HWND) {
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
    check(hwnd, ID_MENU_QUICK, true);
    check(hwnd, ID_MENU_CHECKER, true);
    check(hwnd, ID_PRESERVE_DATE, false);
    check(hwnd, ID_VERBOSE_LOG, false);
    check(hwnd, ID_UPDATE_AUTO, true); // background update check defaults ON
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
unsafe fn reset_formats(hwnd: HWND) {
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
unsafe fn update_quick_enabled(hwnd: HWND) {
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
unsafe fn update_custom_action_enabled(hwnd: HWND) {
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
unsafe fn update_save_dir_enabled(hwnd: HWND) {
    let on = checked(hwnd, ID_SHOT_USE_DIR);
    if let Ok(b) = GetDlgItem(Some(hwnd), ID_SHOT_SET_DIR) {
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(b, on);
    }
    if let Ok(lbl) = GetDlgItem(Some(hwnd), ID_SHOT_DIR) {
        let _ = InvalidateRect(Some(lbl), None, true);
    }
}

unsafe fn banner_rotator(hwnd: HWND) -> Option<(HWND, *mut SponsorRotator)> {
    let banner = GetDlgItem(Some(hwnd), ID_BANNER).ok()?;
    let rot = GetWindowLongPtrW(banner, GWLP_USERDATA) as *mut SponsorRotator;
    (!rot.is_null()).then_some((banner, rot))
}

/// Persist all settings (and re-register formats if the list changed). Apply-only
/// — does NOT close the window, so the user can save and keep tweaking.
unsafe fn apply_settings(hwnd: HWND) {
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
    let shot_on = checked(hwnd, ID_SHOT_ENABLE);
    // set_enabled persists the screenshot flag, then reconciles the daemon (start/stop +
    // re-register) accounting for BOTH the screenshot feature and the custom hotkey saved
    // above — so it covers the "daemon needed only for a custom hotkey" case too.
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
        if !reregister_elevated() {
            for &(ext, _, old) in &changes {
                let _ = settings::set_format_enabled(ext, old);
            }
            message_box(hwnd, t("msg_admin_required"), "SageThumbs 2K");
        }
    }
}

/// Export the saved settings to a user-chosen `.json` file (Diagnostics ▸ Export).
unsafe fn export_settings_to_file(hwnd: HWND) {
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
unsafe fn import_settings_from_file(hwnd: HWND) {
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
unsafe fn refresh_from_settings(hwnd: HWND) {
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
unsafe fn msg(hwnd: HWND, text: &str, caption: &str, icon: MESSAGEBOX_STYLE) {
    let t = wide(text);
    let c = wide(caption);
    MessageBoxW(Some(hwnd), PCWSTR(t.as_ptr()), PCWSTR(c.as_ptr()), MB_OK | icon);
}

/// Clear Windows' thumbnail cache and restart Explorer so thumbnails rebuild. Per-user,
/// no elevation needed (the cache lives in the user's own LocalAppData). Behind a confirm
/// — it briefly blinks the taskbar. This is the fix for the classic "I changed a setting
/// but the thumbnails look the same" (Explorer keeps serving stale cached thumbnails).
unsafe fn rebuild_thumbnail_cache(hwnd: HWND) {
    let warn = wide(
        "This clears Windows' thumbnail cache and briefly restarts File Explorer (your \
         taskbar will blink). Open windows and files are not affected.\n\nContinue?",
    );
    let cap = wide("Rebuild Thumbnail Cache");
    if MessageBoxW(Some(hwnd), PCWSTR(warn.as_ptr()), PCWSTR(cap.as_ptr()), MB_YESNO | MB_ICONWARNING) != IDYES {
        return;
    }
    // Kill Explorer (releases the cache files' lock), delete thumbcache_*.db, relaunch.
    let _ = std::process::Command::new("cmd")
        .args([
            "/c",
            "taskkill /f /im explorer.exe >nul 2>&1 & \
             del /f /q \"%LOCALAPPDATA%\\Microsoft\\Windows\\Explorer\\thumbcache_*.db\" >nul 2>&1 & \
             start \"\" explorer.exe",
        ])
        .spawn();
    msg(
        hwnd,
        "Thumbnail cache cleared and Explorer restarted. Thumbnails will rebuild as you browse.",
        "Rebuild Thumbnail Cache",
        MB_ICONINFORMATION,
    );
}

/// Open the diagnostics log in the user's default text editor (or its folder if the
/// log doesn't exist yet), so a user can find it and send it in for a bug report.
unsafe fn open_diagnostics_log() {
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

/// Re-run `regsvr32` elevated against the installed DLL. `register()` reads the
/// per-extension flags we just wrote, so this brings the HKCR `shellex` keys in
/// line with the Options format list. On an admin account with the silent-
/// elevation policy this raises no prompt.
unsafe fn reregister_elevated() -> bool {
    let dll = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("sagethumbs2k.dll")))
        .unwrap_or_default();
    let params = wide(&format!("/s \"{}\"", dll.display()));
    let verb = wide("runas");
    let file = wide("regsvr32.exe");
    let h = ShellExecuteW(
        Some(HWND::default()),
        PCWSTR(verb.as_ptr()),
        PCWSTR(file.as_ptr()),
        PCWSTR(params.as_ptr()),
        PCWSTR::null(),
        SW_HIDE,
    );
    // ShellExecuteW returns a value > 32 on success; <= 32 means it failed to
    // launch (notably SE_ERR_ACCESSDENIED when the user declines the UAC prompt).
    (h.0 as usize) > 32
}

/// "Repair file associations" — the fix for blank/stuck thumbnails after another program
/// stole SageThumbs' shell hooks (the classic complaint), or an update left them stale.
/// Re-runs the full elevated registration (rewrites every enabled format's thumbnail /
/// context-menu / property hooks back to us), then clears the thumbnail cache + restarts
/// Explorer so the repaired thumbnails render immediately instead of serving stale blanks.
unsafe fn repair_associations(hwnd: HWND) {
    let warn = wide(
        "This re-registers SageThumbs 2K for all your enabled file types — the fix when \
         thumbnails go blank after another program takes over a format — then clears the \
         thumbnail cache and briefly restarts File Explorer (your taskbar will blink).\n\nContinue?",
    );
    let cap = wide("Repair File Associations");
    if MessageBoxW(Some(hwnd), PCWSTR(warn.as_ptr()), PCWSTR(cap.as_ptr()), MB_YESNO | MB_ICONWARNING) != IDYES {
        return;
    }
    if !reregister_elevated() {
        msg(
            hwnd,
            "Couldn't re-register — the elevation prompt was declined or failed. Nothing was changed.",
            "Repair File Associations",
            MB_ICONERROR,
        );
        return;
    }
    // Registration rewrote the hooks; drop the stale cached thumbnails + restart Explorer so
    // the repaired ones render right away. (The cmd sequence gives regsvr32 time to finish.)
    let _ = std::process::Command::new("cmd")
        .args([
            "/c",
            "taskkill /f /im explorer.exe >nul 2>&1 & \
             del /f /q \"%LOCALAPPDATA%\\Microsoft\\Windows\\Explorer\\thumbcache_*.db\" >nul 2>&1 & \
             start \"\" explorer.exe",
        ])
        .spawn();
    msg(
        hwnd,
        "File associations repaired. Thumbnails will rebuild as you browse.",
        "Repair File Associations",
        MB_ICONINFORMATION,
    );
}

// ---- Vertical resize: let the user drag the window taller --------------------------
// The window grows in HEIGHT only (width locked in WM_GETMINMAXINFO). On WM_SIZE the
// bottom-anchored controls slide down / the stretchy ones grow, and the left scroll
// viewport recomputes — so a taller window simply shows more options at once.

struct ReflowCtl {
    id: i32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    stretchy: bool, // true = grow height (top fixed); false = bottom chrome (slide y down)
}
struct ResizeState {
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
unsafe fn on_resize(hwnd: HWND, client_h: i32) {
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

// ===== Settings sync (optional Connections account) =====

/// A background sync op finished on a worker thread → posted back with the boxed outcome
/// (WM_APP + 9; distinct from the update/sponsor app messages at +8/+7).
const WM_APP_SYNC: u32 = 0x8000 + 9;

/// Outcome of a background sync op, boxed through `WM_APP_SYNC` to the UI thread.
enum SyncEvent {
    Connected(Result<String, String>), // Ok(email/sub) or Err(reason)
    Pulled(Result<bool, String>),       // Ok(applied?) or Err(reason)
    Disconnected,
}

enum SyncOp {
    Connect,
    Disconnect,
}

/// The sync button's label: signed out → an invite; signed in → a clean "Stop syncing".
/// The account identity now lives in the status line (see [`sync_status_text`]) — it is
/// deliberately NOT baked into the button anymore (a raw account id read as noise).
fn sync_button_label() -> String {
    if crate::sync_client::is_signed_in() {
        "Stop syncing".to_string()
    } else {
        "Sync settings…".to_string()
    }
}

/// The status line beside the sync button. Signed in → a green "● Synced" badge (the "●…
/// Synced" prefix is what the WM_CTLCOLORSTATIC handler keys the green tint off) with a
/// plain-English detail; signed out → a muted invite. Only a real email is surfaced as the
/// identity — a bare account id (`sub`) is omitted, so the row never shows an ugly UUID.
fn sync_status_text() -> String {
    if crate::sync_client::is_signed_in() {
        match crate::sync_client::signed_in_label() {
            Some(who) if who.contains('@') => format!("● Synced as {who} · up to date"),
            _ => "● Synced · already up to date".to_string(),
        }
    } else {
        "Not syncing — sign in to sync your settings across your PCs".to_string()
    }
}

/// Set the sync button's text + enabled state (used for the transient "Signing in…" state).
unsafe fn set_sync_button(hwnd: HWND, text: &str, enabled: bool) {
    if let Ok(btn) = GetDlgItem(Some(hwnd), ID_SYNC_BTN) {
        let w = wide(text);
        let _ = SetWindowTextW(btn, PCWSTR(w.as_ptr()));
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(btn, enabled);
    }
}

/// Set the sync status line's text. `None` uses the state-derived default (green when it
/// reads "Synced"); `Some` sets a transient line (e.g. "Connecting…"). Repaints so the
/// WM_CTLCOLORSTATIC tint re-evaluates against the new text.
unsafe fn set_sync_status(hwnd: HWND, text: Option<String>) {
    if let Ok(h) = GetDlgItem(Some(hwnd), ID_SYNC_STATUS) {
        let t = text.unwrap_or_else(sync_status_text);
        let w = wide(&t);
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
        let _ = InvalidateRect(Some(h), None, true);
    }
}

/// Reconcile the whole sync row (button label + status badge) with the current signed-in
/// state. Called on load and after every finished sync op.
unsafe fn refresh_sync_ui(hwnd: HWND) {
    set_sync_button(hwnd, &sync_button_label(), true);
    set_sync_status(hwnd, None);
}

/// The sync button was clicked: sign in (with a plain-English disclosure that doubles as
/// the privacy notice) or disconnect. The network op itself runs on a worker thread.
unsafe fn on_sync_click(hwnd: HWND) {
    if crate::sync_client::is_signed_in() {
        let warn = wide(
            "Stop syncing and disconnect this account?\n\nYour settings stay on this PC; the \
             copy stored in your Connections account is removed.",
        );
        let cap = wide("Settings Sync");
        if MessageBoxW(Some(hwnd), PCWSTR(warn.as_ptr()), PCWSTR(cap.as_ptr()), MB_YESNO | MB_ICONWARNING)
            != IDYES
        {
            return;
        }
        set_sync_button(hwnd, "Disconnecting…", false);
        set_sync_status(hwnd, Some("Disconnecting…".to_string()));
        spawn_sync(hwnd, SyncOp::Disconnect);
    } else {
        let info = wide(
            "Sign in with a Connections account to sync your SageThumbs 2K preferences across \
             your PCs.\n\nOnly settings sync — never your files, folder paths, or passwords. \
             It's optional, and you can disconnect anytime.\n\nA browser window will open for \
             you to sign in. Continue?",
        );
        let cap = wide("Settings Sync");
        if MessageBoxW(Some(hwnd), PCWSTR(info.as_ptr()), PCWSTR(cap.as_ptr()), MB_YESNO | MB_ICONINFORMATION)
            != IDYES
        {
            return;
        }
        set_sync_button(hwnd, "Signing in… (see your browser)", false);
        set_sync_status(hwnd, Some("Connecting…".to_string()));
        spawn_sync(hwnd, SyncOp::Connect);
    }
}

/// Run a connect/disconnect on a worker thread (they block on the network), posting the
/// result back via `WM_APP_SYNC` so the UI updates on the message thread.
fn spawn_sync(hwnd: HWND, op: SyncOp) {
    let target = hwnd.0 as isize;
    std::thread::spawn(move || {
        let event = match op {
            SyncOp::Connect => SyncEvent::Connected(crate::sync_client::connect()),
            SyncOp::Disconnect => {
                crate::sync_client::disconnect();
                SyncEvent::Disconnected
            }
        };
        post_sync(target, event);
    });
}

/// On Settings open, if signed in, pull the cloud copy in the background.
fn spawn_sync_pull(hwnd: HWND) {
    if !crate::sync_client::is_signed_in() {
        return;
    }
    let target = hwnd.0 as isize;
    std::thread::spawn(move || {
        post_sync(target, SyncEvent::Pulled(crate::sync_client::pull_on_open()));
    });
}

/// After Save, if signed in, mirror the local settings to the cloud (fire-and-forget).
fn spawn_sync_push(_hwnd: HWND) {
    if !crate::sync_client::is_signed_in() {
        return;
    }
    std::thread::spawn(|| {
        let _ = crate::sync_client::push();
    });
}

/// Post a boxed `SyncEvent` to the window; reclaim the box if the window is already gone.
fn post_sync(target: isize, event: SyncEvent) {
    let raw = Box::into_raw(Box::new(event));
    unsafe {
        let posted = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(HWND(target as *mut core::ffi::c_void)),
            WM_APP_SYNC,
            WPARAM(0),
            LPARAM(raw as isize),
        );
        if posted.is_err() {
            drop(Box::from_raw(raw));
        }
    }
}

/// Apply a finished sync op to the UI (runs on the message thread).
unsafe fn handle_sync_event(hwnd: HWND, event: SyncEvent) {
    match event {
        SyncEvent::Connected(Ok(who)) => {
            refresh_sync_ui(hwnd);
            msg(
                hwnd,
                &format!(
                    "Signed in as {who}.\n\nYour settings now sync across your PCs. Anything \
                     synced from another device has been applied — reopen Settings to see it \
                     reflected here."
                ),
                "Settings Sync",
                MB_ICONINFORMATION,
            );
        }
        SyncEvent::Connected(Err(e)) => {
            refresh_sync_ui(hwnd);
            msg(hwnd, &format!("Couldn't sign in: {e}"), "Settings Sync", MB_ICONWARNING);
        }
        SyncEvent::Pulled(res) => {
            // Background pull: settle the row. Applied values are already in HKCU (they take
            // effect for new thumbnails); we don't force a reopen or nag — just reflect
            // whether the pull pulled anything new in the status badge.
            set_sync_button(hwnd, &sync_button_label(), true);
            match res {
                Ok(true) => set_sync_status(hwnd, Some("● Synced · updated from another device".to_string())),
                _ => set_sync_status(hwnd, None), // "● Synced · already up to date"
            }
        }
        SyncEvent::Disconnected => {
            refresh_sync_ui(hwnd);
            msg(hwnd, "Sync disconnected. Your settings remain on this PC.", "Settings Sync", MB_ICONINFORMATION);
        }
    }
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
                            // deletes + reinserts all 316 rows. Skip that whole rebuild
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
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

// ===================== Shared owner-draw helpers =========================
// The owner-draw painting lives in the `restyle` and `scroll` submodules and runs
// in BOTH themes — light mode is a recolored clone of dark (the palette fns like
// `SURFACE`/`BORDER`/`INPUT_BG` are theme-aware), NOT the native light dialog, so
// the entry points are NOT `is_dark()`-gated. (A few genuinely dark-only bits — e.g.
// the native dark item-view theme — still check `is_dark()` at their own call site.)
// These few small helpers stay in the parent because more than one cluster needs
// them: `s` and `fill` are used by both `restyle` and `scroll`, and `control_text` /
// `is_button_class` back the button/header painters.

/// 96-DPI design pixels → device pixels for this window's DPI.
unsafe fn s(hwnd: HWND, v: i32) -> i32 {
    dpi_scale(hwnd, v)
}

/// Fill `rc` with a flat `color` using the stock DC brush (no allocation).
unsafe fn fill(hdc: HDC, rc: &RECT, color: COLORREF) {
    SetDCBrushColor(hdc, color);
    FillRect(hdc, rc, HBRUSH(GetStockObject(DC_BRUSH).0));
}

/// A control's window text as a NUL-terminated wide buffer.
unsafe fn control_text(h: HWND) -> Vec<u16> {
    let n = GetWindowTextLengthW(h).max(0) as usize;
    let mut buf = vec![0u16; n + 1];
    let got = GetWindowTextW(h, &mut buf).max(0) as usize;
    buf.truncate(got + 1);
    buf
}

/// True when `h` is a standard BUTTON-class control — so an NM_CUSTOMDRAW from it
/// is ours to paint (as opposed to, e.g., the SysLink credit, which isn't).
unsafe fn is_button_class(h: HWND) -> bool {
    let mut buf = [0u16; 16];
    let n = GetClassNameW(h, &mut buf);
    n > 0 && String::from_utf16_lossy(&buf[..n as usize]).eq_ignore_ascii_case("button")
}

// ---- Small ListView check helpers --------------------------------------

unsafe fn set_check(list: HWND, item: i32, on: bool) {
    let st = LVITEMW {
        state: LIST_VIEW_ITEM_STATE_FLAGS(if on { CHECKED } else { UNCHECKED }),
        stateMask: LVIS_STATEIMAGEMASK,
        ..Default::default()
    };
    SendMessageW(
        list,
        LVM_SETITEMSTATE,
        Some(WPARAM(item as usize)),
        Some(LPARAM(&st as *const _ as isize)),
    );
}
/// Remove a row's checkbox glyph (state image 0) — used for the menu list's divider rows.
pub(super) unsafe fn clear_checkbox(list: HWND, item: i32) {
    let st = LVITEMW {
        state: LIST_VIEW_ITEM_STATE_FLAGS(0),
        stateMask: LVIS_STATEIMAGEMASK,
        ..Default::default()
    };
    SendMessageW(
        list,
        LVM_SETITEMSTATE,
        Some(WPARAM(item as usize)),
        Some(LPARAM(&st as *const _ as isize)),
    );
}
unsafe fn is_checked(list: HWND, item: i32) -> bool {
    let st = SendMessageW(
        list,
        LVM_GETITEMSTATE,
        Some(WPARAM(item as usize)),
        Some(LPARAM(LVIS_STATEIMAGEMASK.0 as isize)),
    );
    (st.0 as u32 & 0x3000) == CHECKED
}

// ===== Headless self-capture (verification + README/site assets) =====

/// Build the Settings window OFF-SCREEN (invisible, steals no focus), returning the HWND once
/// its controls have realized + painted. Shared by `run_shot` (one pane → PNG) and
/// `run_shot_gif` (all panes → animated GIF).
unsafe fn build_settings_shot_window(hinst: HINSTANCE, dark: bool) -> Option<HWND> {
    let hwnd = crate::win::create_shot_window(
        hinst,
        dark,
        w!("SageThumbs2KOptions"),
        Some(wndproc),
        "SageThumbs 2K — Settings",
        772,
        588,
    )?;
    // Let the controls — the ListView especially — realize + paint before we drive panes.
    crate::win::pump_msgs(20);
    Some(hwnd)
}

/// Switch to category `tab` and settle it for capture. The shared owner-drawn chrome (nav
/// rail, pane header, footer buttons) only fully repaints on a *real* category transition —
/// re-selecting the current tab is a no-op that leaves them blank in a headless grab. So we
/// PRIME with a switch to a different tab first, making the switch to `tab` a real transition,
/// then double `RDW_UPDATENOW` around pumps so every control has actually painted.
unsafe fn settle_pane(hwnd: HWND, tab: usize) {
    let tab = tab.min(NCAT - 1);
    let prime = if tab == 0 { NCAT - 1 } else { 0 };
    switch_category(hwnd, prime);
    crate::win::pump_msgs(5);
    switch_category(hwnd, tab);
    crate::win::force_repaint(hwnd);
    crate::win::pump_msgs(12);
    crate::win::force_repaint(hwnd);
    crate::win::pump_msgs(4);
}

/// The app's `--shot` mode: build the Settings window off-screen, switch to category `tab`,
/// render it to a PNG at `out` via `PrintWindow`, then tear it down. Lets a UI change be
/// screenshotted programmatically — no window ever appears and the desktop is never driven.
/// Returns whether the PNG was written.
pub(crate) unsafe fn run_shot(hinst: HINSTANCE, dark: bool, out: &str, tab: usize) -> bool {
    let Some(hwnd) = build_settings_shot_window(hinst, dark) else {
        return false;
    };
    settle_pane(hwnd, tab);
    let ok = crate::screenshot::capture_hwnd_to_png(hwnd, std::path::Path::new(out));
    let _ = DestroyWindow(hwnd);
    ok
}

/// The app's `--shot-gif` mode: build the Settings window off-screen ONCE, walk every category
/// tab capturing each as a frame, and encode them into an animated (infinite-loop) GIF at
/// `out` — the regenerable README/site asset that cycles the Settings tabs. Frames are
/// downscaled to the 96-dpi design width so the GIF stays crisp + small. Returns whether the
/// GIF was written.
pub(crate) unsafe fn run_shot_gif(_hinst: HINSTANCE, _dark: bool, out: &str) -> bool {
    // Capture each frame in a FRESH PROCESS (`--shot --tab N`), then assemble the GIF from the
    // PNGs. This is deliberate: the single-shot path is the ONLY one that reliably renders every
    // tab correctly. Reusing one window across tabs in-process raced the owner-drawn nav-rail
    // highlight (the capture grabbed the PREVIOUS tab's selection off a trailing DWM surface),
    // and churning fresh windows in a tight in-process loop left some frames blank (each window
    // hadn't finished painting). A separate, fully-initialized process per frame sidesteps both.
    // ~1 s each; this is a rarely-run asset-regen path, so the extra spawns don't matter.
    use std::os::windows::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let tmp = std::env::temp_dir();
    let pid = std::process::id();
    let mut frames = Vec::with_capacity(NCAT);
    for tab in 0..NCAT {
        let png = tmp.join(format!("st2k_gifframe_{pid}_{tab}.png"));
        let Some(png_s) = png.to_str() else { continue };
        let ok = std::process::Command::new(&exe)
            .args(["--shot", png_s, "--tab", &tab.to_string()])
            .creation_flags(sagethumbs2k_core::CREATE_NO_WINDOW)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            if let Ok(img) = image::open(&png) {
                frames.push(crate::screenshot::downscale_to_width(img.to_rgba8(), 772));
            }
        }
        let _ = std::fs::remove_file(&png);
    }
    if frames.is_empty() {
        return false;
    }
    // ~1.6 s per tab so a reader can take each pane in before it advances.
    crate::screenshot::encode_gif(&frames, std::path::Path::new(out), 1600)
}
