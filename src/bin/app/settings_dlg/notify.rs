//! WM_NOTIFY: the format list's drag, custom draw and item changes, plus links and tooltips.

use super::*;

pub(super) unsafe fn on_notify(hwnd: HWND, lparam: LPARAM) -> LRESULT {
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
pub(super) unsafe fn on_notify_begindrag(
    hwnd: HWND,
    nmhdr: *const NMHDR,
    lparam: LPARAM,
) -> Option<LRESULT> {
    if (*nmhdr).code == windows::Win32::UI::Controls::LVN_BEGINDRAG
        && (*nmhdr).hwndFrom == GetDlgItem(Some(hwnd), ID_MENU_ITEMS_LIST).unwrap_or_default()
    {
        let nmlv = lparam.0 as *const NMLISTVIEW;
        list::begin_menu_drag((*nmhdr).hwndFrom, (*nmlv).iItem);
        return Some(LRESULT(0));
    }
    None
}

pub(super) unsafe fn on_notify_customdraw(hwnd: HWND, lparam: LPARAM) -> LRESULT {
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
pub(super) unsafe fn on_notify_itemchanged(
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

pub(super) unsafe fn on_notify_link_or_tip(
    hwnd: HWND,
    nmhdr: *const NMHDR,
    lparam: LPARAM,
) -> LRESULT {
    let code = (*nmhdr).code;
    if code == NM_CLICK || code == NM_RETURN {
        crate::win::open_notify_link(lparam.0 as *const NMLINK);
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
