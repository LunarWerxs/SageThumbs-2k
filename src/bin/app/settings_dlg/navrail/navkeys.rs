//! A rail item as a control: its keyboard, its accessibility facts and the subclass procedure that serves both.

use super::*;

/// The nav rail's UIA hookup (see `uia.rs`'s module doc for the technique). Rows read as UIA
/// **list items**, not buttons: the rail is a single-select list of destinations where exactly
/// one is always "selected" (the accent pill a sighted user sees), so `ISelectionItemProvider` —
/// "General, list item, 1 of 11, selected" — says what a click here actually does. A plain
/// `Button`/`Invoke` role would say "activate this" with no sense that picking one deselects the
/// last one, which is the opposite of how the rail behaves.
pub(super) static NAV_ITEM_UIA_OPS: uia::ItemOps = uia::ItemOps {
    describe: nav_item_uia_facts,
    select: nav_item_uia_select,
};

/// The category index a nav-rail item's own control id encodes, bounds-checked because this
/// runs off a UIA provider call rather than a click the rail itself generated.
pub(super) fn nav_ci_of(h: HWND) -> Option<usize> {
    let ci = unsafe { GetDlgCtrlID(h) } - ID_NAV_BASE;
    (0..NCAT as i32).contains(&ci).then_some(ci as usize)
}

/// The pure part of a row's UIA description: given which category `ci` is, whether the rail's
/// stored `active` category is the same one, and whether THIS row currently holds keyboard
/// focus, what should a screen reader be told. Split out from `nav_item_uia_facts` (which also
/// needs a live `HWND` to resolve `ci` and to read real keyboard focus) so the mapping itself is
/// unit-testable without a window.
pub(super) fn nav_item_facts_for(ci: usize, active: usize, focused: bool) -> uia::ItemFacts {
    uia::ItemFacts {
        name: nav_label(ci).to_string(),
        automation_id: nav_key(ci).to_string(),
        control_type: UIA_ListItemControlTypeId,
        enabled: true,
        focused,
        selected: active == ci,
    }
}

pub(super) fn nav_item_uia_facts(h: HWND) -> Option<uia::ItemFacts> {
    let ci = nav_ci_of(h)?;
    let active = NAV.with(|n| n.borrow().active);
    Some(nav_item_facts_for(ci, active, unsafe { GetFocus() } == h))
}

/// `ISelectionItemProvider::Select` on a row: the same effect a click has.
pub(super) fn nav_item_uia_select(h: HWND) {
    let Some(ci) = nav_ci_of(h) else { return };
    let Ok(parent) = (unsafe { GetParent(h) }) else {
        return;
    };
    unsafe { switch_category(parent, ci) };
}

/// Enter/Space activate a focused nav row.
pub(super) fn nav_key_activates(vk: u16) -> bool {
    vk == VK_RETURN.0 || vk == VK_SPACE.0
}

/// Up/Down move focus to the neighbouring nav row and switch to it immediately, matching the
/// click path. Returns `None` for any other key so the caller can fall through.
pub(super) unsafe fn nav_key_arrow(parent: HWND, ci: usize, vk: u16) -> Option<LRESULT> {
    let next = if vk == VK_UP.0 {
        (ci + NCAT - 1) % NCAT
    } else if vk == VK_DOWN.0 {
        (ci + 1) % NCAT
    } else {
        return None;
    };
    if let Ok(target) = GetDlgItem(Some(parent), ID_NAV_BASE + next as i32) {
        let _ = SetFocus(Some(target));
    }
    switch_category(parent, next);
    Some(LRESULT(0))
}

/// `WM_KEYDOWN` on a nav row: Enter/Space switches to the row's page, Up/Down moves focus and
/// switches. Returns `None` for keys we don't handle (or a row without a parent).
pub(super) unsafe fn nav_item_keydown(h: HWND, w: WPARAM) -> Option<LRESULT> {
    let vk = w.0 as u16;
    if let Ok(parent) = GetParent(h) {
        let ci = (GetDlgCtrlID(h) - ID_NAV_BASE) as usize;
        if nav_key_activates(vk) {
            switch_category(parent, ci);
            return Some(LRESULT(0));
        }
        return nav_key_arrow(parent, ci, vk);
    }
    None
}

/// Keyboard access for a nav-rail item (the rail used to be mouse-only: SS_OWNERDRAW|SS_NOTIFY
/// statics with no WS_TABSTOP and no key handling at all).
///
/// `WM_GETDLGCODE` claims arrows AND "all keys" so `IsDialogMessageW` (the message-pump helper
/// `run_dialog` uses — see `win::mod.rs`) hands us Up/Down/Enter/Space instead of either moving
/// focus itself or treating Enter as a click on the dialog's default button.
pub(super) unsafe extern "system" fn nav_item_subclass(
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
            // Retire this row's UIA provider before the window it describes goes away — see
            // `uia::on_destroy`'s own doc for why that is not optional.
            unsafe { uia::on_destroy(h) };
        }
        // A screen reader's `WM_GETOBJECT` for this row; see `uia.rs`'s module doc. Declined
        // (falls through to `DefSubclassProc`) for anything that isn't the UIA root request.
        WM_GETOBJECT => {
            if let Some(r) = unsafe { uia::on_get_object(h, w, l, &NAV_ITEM_UIA_OPS) } {
                return r;
            }
        }
        uia::WM_UIA_JOB => return unsafe { uia::run_job(h, l) },
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
            if let Some(r) = nav_item_keydown(h, w) {
                return r;
            }
        }
        _ => {}
    }
    DefSubclassProc(h, msg, w, l)
}
