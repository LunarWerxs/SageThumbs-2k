//! The "Menu items" popup editor — the drag-to-reorder visibility checklist in its own
//! small modal window instead of squeezed into the Right-click menu page.
//!
//! On that page the list was the reason everything felt cramped: it competed for height
//! with the page's own sections and had to be capped so the footer stayed visible. A
//! popup gives it the one thing it actually needs — room — and gives the page back its
//! breathing space.
//!
//! Implementation: the checklist control is NOT rebuilt here. The real ListView (still
//! created by `build_controls`, hidden by `apply_v3_layout`) is RE-PARENTED into the
//! popup for the dialog's lifetime and re-parented back on close. Everything that reads
//! it — `load_values` seeding checks, `apply_settings` persisting visibility + order,
//! `list::` drag-reorder state — keeps working on the same HWND with zero duplication.
//! The popup is modal over Settings, so nothing can read the list while it is away.

use super::*;
use crate::win::IDOK;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;

// The Settings window, so the close path can hand the list back. Set for the popup's
// lifetime only.
thread_local! {
    static OWNER: std::cell::Cell<Option<isize>> = const { std::cell::Cell::new(None) };
}

const POP_W: i32 = 400;
const POP_H: i32 = 470;
const M: i32 = 14;
// NOT 1: that is IDOK, and a colliding id would route the Done button into Reset.
const ID_POP_RESET: i32 = 100;
// IDOK (the "Done" button) comes from crate::win.

/// Open the editor, modal over `settings`. Returns after it closes.
pub(super) unsafe fn open(settings: HWND) {
    OWNER.with(|o| o.set(Some(settings.0 as isize)));
    crate::win::run_dialog(
        w!("SageThumbs2KMenuItems"),
        Some(popup_wndproc),
        t("grp_menu_items"),
        POP_W,
        POP_H,
        Some(settings),
    );
    OWNER.with(|o| o.set(None));
}

unsafe fn settings_hwnd() -> Option<HWND> {
    OWNER.with(|o| o.get()).map(|p| HWND(p as *mut _))
}

/// Take the checklist from Settings, fill the popup with it, and add Reset/Done.
unsafe fn build(hwnd: HWND, hinst: HINSTANCE) {
    use crate::win::{ctl, dpi_scale, BUTTON, IDOK};
    let Some(settings) = settings_hwnd() else {
        return;
    };
    let Ok(list) = GetDlgItem(Some(settings), ID_MENU_ITEMS_LIST) else {
        return;
    };
    let _ = SetParent(list, Some(hwnd));

    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let unit = dpi_scale(hwnd, 100).max(1);
    let cw = (rc.right - rc.left) * 100 / unit;
    let ch = (rc.bottom - rc.top) * 100 / unit;
    let btn_h = 28;
    let list_h = ch - M * 2 - btn_h - 10;
    let _ = SetWindowPos(
        list,
        None,
        dpi_scale(hwnd, M),
        dpi_scale(hwnd, M),
        dpi_scale(hwnd, cw - M * 2),
        dpi_scale(hwnd, list_h),
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
    // The list arrives with whatever exact-fit height the page gave it; rounded corners
    // keep it in the family of the other beta surfaces.
    super::restyle::round_corners(hwnd, list, 8);
    let _ = ShowWindow(list, SW_SHOW);

    let by = ch - M - btn_h;
    ctl(
        hwnd,
        BUTTON,
        t("btn_menu_reset"),
        WINDOW_STYLE(0) | WS_TABSTOP,
        M,
        by,
        110,
        btn_h,
        ID_POP_RESET,
        hinst,
    );
    ctl(
        hwnd,
        BUTTON,
        t("btn_done"),
        WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
        cw - M - 96,
        by,
        96,
        btn_h,
        IDOK,
        hinst,
    );
}

extern "system" fn popup_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if let Some(r) = crate::dark::dark_ctlcolor(msg, wparam) {
            return r;
        }
        match msg {
            WM_CREATE => {
                let hinst: HINSTANCE = match GetModuleHandleW(None) {
                    Ok(h) => h.into(),
                    Err(_) => return LRESULT(-1),
                };
                build(hwnd, hinst);
                LRESULT(0)
            }
            // The checklist's notifications now land HERE, not on the Settings wndproc —
            // forward the two it needs: drag-to-reorder and the dark-mode button paint.
            WM_NOTIFY => {
                let nmhdr = lparam.0 as *const NMHDR;
                let code = (*nmhdr).code;
                if code == windows::Win32::UI::Controls::LVN_BEGINDRAG
                    && (*nmhdr).hwndFrom
                        == GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST).unwrap_or_default()
                {
                    let nmlv = lparam.0 as *const NMLISTVIEW;
                    list::begin_menu_drag((*nmhdr).hwndFrom, (*nmlv).iItem);
                    return LRESULT(0);
                }
                if code == NM_CUSTOMDRAW {
                    if is_button_class((*nmhdr).hwndFrom) {
                        return LRESULT(restyle::draw_button_cd(
                            hwnd,
                            lparam.0 as *const NMCUSTOMDRAW,
                        ));
                    }
                    return LRESULT(CDRF_DODEFAULT as isize);
                }
                LRESULT(0)
            }
            // Right-click bulk toggle (check all / uncheck all) on the checklist. Gated on
            // the sender being the list itself — WM_CONTEXTMENU also fires for Reset/Done
            // and for the popup's own background, and without this guard any of those
            // right-clicks popped the list's Check/Uncheck/Toggle menu too. Mirrors the
            // same guard on the format list's own WM_CONTEXTMENU in mod.rs.
            WM_CONTEXTMENU
                if HWND(wparam.0 as *mut c_void)
                    == GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST).unwrap_or_default() =>
            {
                list::list_context_menu(HWND(wparam.0 as *mut c_void), hwnd, lparam);
                LRESULT(0)
            }
            // Owner-drawn dark context-menu items (light text on dark). TrackPopupMenu
            // routes WM_MEASUREITEM/WM_DRAWITEM to its `hOwner` argument, which
            // `list::list_context_menu` passes as THIS popup (not Settings), so the popup
            // wndproc must handle them itself — mod.rs's ODT_MENU arms did this for the
            // format list's identical dark-mode menu, but this window never inherited it.
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
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            WM_COMMAND => {
                match (wparam.0 & 0xFFFF) as i32 {
                    ID_POP_RESET => {
                        if let Ok(list) = GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST) {
                            list::reset_menu_order(list);
                        }
                    }
                    IDOK => {
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                // Hand the checklist back BEFORE the window goes away, hidden again, so
                // load/save find it exactly where they always did.
                if let (Some(settings), Ok(list)) =
                    (settings_hwnd(), GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST))
                {
                    let _ = ShowWindow(list, SW_HIDE);
                    let _ = SetParent(list, Some(settings));
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
